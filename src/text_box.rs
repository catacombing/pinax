//! Text input area.

use std::f32::consts::SQRT_2;
use std::io::{ErrorKind as IoErrorKind, Read, Write};
use std::ops::{Bound, Range, RangeBounds};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use std::{cmp, fs, mem};

use calloop::timer::{TimeoutAction, Timer};
use calloop::{LoopHandle, RegistrationToken};
use calloop_notify::NotifySource;
use calloop_notify::notify::{EventKind, RecursiveMode, Watcher};
use skia_safe::textlayout::{
    FontCollection, LineMetrics, Paragraph, ParagraphBuilder, ParagraphStyle, TextDecoration,
    TextStyle,
};
use skia_safe::{
    Canvas as SkiaCanvas, Color4f, Font, FontMetrics, FontMgr, Paint, Path, Point, Rect,
};
use smithay_client_toolkit::seat::keyboard::{Keysym, Modifiers};
use tempfile::NamedTempFile;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::geometry::{Position, Size};
use crate::window::{BULLET_POINT_PADDING, BULLET_POINT_SIZE};
use crate::{Error, State};

// Selection caret size at scale 1.
const CARET_SIZE: f64 = 5.;

// Caret outline width at scale 1.
const CARET_STROKE: f64 = 3.;

/// Maximum number of surrounding bytes submitted to IME.
///
/// The value `4000` is chosen to match the maximum Wayland protocol message
/// size, a higher value will lead to errors.
const MAX_SURROUNDING_BYTES: usize = 4000;

/// An area for text input.
pub struct TextBox {
    event_loop: LoopHandle<'static, State>,

    fallback_metrics: Option<FontMetrics>,
    font_collection: FontCollection,
    selection_style: TextStyle,
    text_style: TextStyle,
    selection_paint: Paint,
    paint: Paint,

    last_paragraph: Option<Paragraph>,
    last_cursor_rect: Option<Rect>,
    last_paragraph_height: f32,

    preedit_text: String,
    text: String,

    selection: Option<Range<usize>>,
    cursor_index: usize,

    size: Size,
    scale: f64,

    font_family: String,
    font_size: f64,

    touch_state: TouchState,
    scroll_offset: f32,

    keyboard_focused: bool,
    ime_focused: bool,

    persist_token: Option<RegistrationToken>,
    persist_start: Option<Instant>,
    storage_path: PathBuf,

    focus_cursor: bool,

    text_input_dirty: bool,
    dirty: bool,
}

impl TextBox {
    pub fn new(event_loop: LoopHandle<'static, State>, config: &Config) -> Result<Self, Error> {
        let font_family = config.font.family.clone();
        let font_size = config.font.size;

        let mut paint = Paint::default();
        paint.set_color4f(config.colors.foreground.as_color4f(), None);
        paint.set_anti_alias(true);

        let mut text_style = TextStyle::new();
        text_style.set_foreground_paint(&paint);
        text_style.set_font_size(font_size as f32);
        text_style.set_font_families(&[&font_family]);

        let mut selection_paint = paint.clone();
        selection_paint.set_stroke_width(CARET_STROKE as f32);
        let mut selection_style = text_style.clone();
        selection_paint.set_color4f(config.colors.background.as_color4f(), None);
        selection_style.set_foreground_paint(&selection_paint);
        selection_paint.set_color4f(config.colors.highlight.as_color4f(), None);
        selection_style.set_background_paint(&selection_paint);

        let mut font_collection = FontCollection::new();
        font_collection.set_default_font_manager(FontMgr::new(), None);

        // Read initial text from file.
        let storage_path = config.general.storage_path();
        let text = Self::read_to_string(&storage_path).unwrap_or_default();
        let cursor_index = text.len();

        // Update text box on file change.
        Self::monitor_file(&event_loop, storage_path.clone())?;

        Ok(Self {
            font_collection,
            selection_paint,
            selection_style,
            cursor_index,
            storage_path,
            font_family,
            event_loop,
            text_style,
            font_size,
            paint,
            text,
            text_input_dirty: true,
            dirty: true,
            scale: 1.,
            last_paragraph_height: Default::default(),
            fallback_metrics: Default::default(),
            keyboard_focused: Default::default(),
            last_cursor_rect: Default::default(),
            last_paragraph: Default::default(),
            persist_start: Default::default(),
            persist_token: Default::default(),
            scroll_offset: Default::default(),
            focus_cursor: Default::default(),
            preedit_text: Default::default(),
            ime_focused: Default::default(),
            touch_state: Default::default(),
            selection: Default::default(),
            size: Default::default(),
        })
    }

    /// Check whether the text box requires a redraw.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Retrieve and reset current IME dirtiness state.
    pub fn take_text_input_dirty(&mut self) -> bool {
        mem::take(&mut self.text_input_dirty)
    }

    /// Render text content to the canvas.
    pub fn draw(&mut self, canvas: &SkiaCanvas, point: impl Into<Point>) {
        let mut point = point.into();

        self.dirty = false;

        // Render text if not empty.
        if !self.text.is_empty() || !self.preedit_text.is_empty() {
            // Re-layout paragraph content.
            self.update_paragraph();

            // Scroll to cursor, or clamp offset within maximum bounds.
            if mem::take(&mut self.focus_cursor) {
                unsafe { self.update_scroll_offset() };
            } else {
                unsafe { self.clamp_scroll_offset() };
            }

            let paragraph = self.last_paragraph.as_ref().unwrap();

            // Render text.
            point.y += (self.size.height as f32 - self.last_paragraph_height).max(0.);
            point.y += self.scroll_offset;
            paragraph.paint(canvas, point);

            // Draw list element bullet points.
            self.draw_bullet_points(canvas, point);
        } else {
            // Reset scroll offset if there is no text.
            self.scroll_offset = 0.;

            // Calculate approximate line height.
            let metrics = self.fallback_metrics();
            self.last_paragraph_height = metrics.descent - metrics.ascent;
            self.last_paragraph = None;

            // Anchor content to the bottom of the window.
            point.y += (self.size.height as f32 - self.last_paragraph_height).max(0.);

            // Draw list element bullet points.
            self.draw_bullet_points(canvas, point);
        }

        // Draw cursor or selection carets while focused.
        self.last_cursor_rect =
            (self.keyboard_focused || self.ime_focused).then(|| self.draw_cursor(canvas, point));
    }

    /// Draw input or selection cursors.
    fn draw_cursor(&mut self, canvas: &SkiaCanvas, point: Point) -> Rect {
        match self.selection {
            Some(Range { start, end }) => {
                // Get points required for drawing the triangles.
                let (start_points, line_height) = self.caret_points(point, start);
                let start_path = Path::polygon(&start_points, true, None, true);
                let (end_points, _) = self.caret_points(point, end);
                let end_path = Path::polygon(&end_points, true, None, true);

                // Draw the caret outlines.
                self.selection_paint.set_stroke(true);
                canvas.draw_path(&start_path, &self.selection_paint);
                canvas.draw_path(&end_path, &self.selection_paint);
                self.selection_paint.set_stroke(false);

                // Draw the center/background.
                canvas.draw_path(&start_path, &self.selection_style.foreground());
                canvas.draw_path(&end_path, &self.selection_style.foreground());

                // Use entire selection as IME cursor rectangle.
                let start = start_points[2];
                let end = end_points[2];
                Rect::new(start.x, start.y, end.x, end.y + line_height)
            },
            None => {
                // Get metrics at cursor position.
                let metrics = self.metrics_at(self.cursor_index);

                // Calculate cursor bounding box.
                let x = point.x + metrics.x;
                let y = point.y + metrics.baseline - metrics.ascent;
                let width = self.scale.round() as f32;
                let height = (metrics.ascent + metrics.descent).round();

                // Render the cursor rectangle.
                let rect = Rect::new(x, y, x + width, y + height);
                canvas.draw_rect(rect, &self.paint);

                rect
            },
        }
    }

    /// Draw list bullet points.
    fn draw_bullet_points(&mut self, canvas: &SkiaCanvas, origin: Point) {
        match self.last_paragraph.as_ref() {
            Some(paragraph) => {
                // Add bullet points in front of list elements.
                let mut consecutive_newlines = 2;
                for (i, c) in self.text.char_indices() {
                    if c == '\n' {
                        consecutive_newlines += 1;
                        continue;
                    } else if c.is_whitespace() {
                        continue;
                    }

                    // Draw bullet points after at least one empty line.
                    if consecutive_newlines >= 2 {
                        // Get metrics of the first character in the line.
                        let line = paragraph.get_line_number_at(i).unwrap();
                        let metrics = paragraph.get_line_metrics_at(line).unwrap();

                        // Draw rectangle in the padding area.
                        let size = BULLET_POINT_SIZE * self.scale as f32;
                        let y = origin.y + metrics.baseline as f32 - metrics.ascent as f32 / 2.
                            + metrics.descent as f32 / 2.
                            - size / 2.;
                        let x = origin.x - BULLET_POINT_PADDING * self.scale as f32;
                        let rect = Rect::new(x, y, x + size, y + size);
                        canvas.draw_rect(rect, &self.paint);
                    }

                    consecutive_newlines = 0;
                }
            },
            None => {
                // Handle bullet point drawing without any text.
                let size = BULLET_POINT_SIZE * self.scale as f32;
                let y = origin.y + self.last_paragraph_height / 2. - size / 2.;
                let x = origin.x - BULLET_POINT_PADDING * self.scale as f32;
                let rect = Rect::new(x, y, x + size, y + size);
                canvas.draw_rect(rect, &self.paint);
            },
        }
    }

    /// Update the text paragraph layout.
    fn update_paragraph(&mut self) {
        // Get selection range, defaulting to an empty selection.
        let selection = match self.selection.as_ref() {
            Some(selection) => selection.start..selection.end,
            None => self.text.len()..usize::MAX,
        };

        // Create paragraph builder with the default text style.
        let mut paragraph_style = ParagraphStyle::new();
        paragraph_style.set_text_style(&self.text_style);
        let mut paragraph_builder = ParagraphBuilder::new(&paragraph_style, &self.font_collection);

        // Draw text before the selection, or entire text without selection.
        if selection.start > 0 {
            paragraph_builder.add_text(&self.text[..selection.start]);
        }

        // Draw selection and text after it.
        if selection.start < self.text.len() {
            paragraph_builder.push_style(&self.selection_style);
            paragraph_builder.add_text(&self.text[selection.start..selection.end]);

            paragraph_builder.pop();
            paragraph_builder.add_text(&self.text[selection.end..]);
        }

        // Add preedit text with underline.
        if !self.preedit_text.is_empty() {
            // Create style with reduced text brightness and underline.
            let color = Color4f { a: 0.6, ..self.paint.color4f() };
            self.paint.set_color4f(color, None);
            let mut text_style = self.text_style.clone();
            text_style.set_decoration_type(TextDecoration::UNDERLINE);
            text_style.set_foreground_paint(&self.paint);

            // Add styled text to the paragraph.
            paragraph_builder.push_style(&text_style);
            paragraph_builder.add_text(&self.preedit_text);
        }

        // Build paragraph and calculate its height.
        let mut paragraph = paragraph_builder.build();
        paragraph.layout(self.size.width as f32);

        self.last_paragraph_height = paragraph.height();
        self.last_paragraph = Some(paragraph);
    }

    /// Set the text box's physical size.
    pub fn set_size(&mut self, size: Size) {
        if self.size == size {
            return;
        }
        self.size = size;

        // Ensure cursor is visible after resize.
        self.focus_cursor = true;

        self.dirty = true;
    }

    /// Set the text box's font scale.
    pub fn set_scale_factor(&mut self, scale: f64) {
        if self.scale == scale {
            return;
        }
        self.scale = scale;
        self.dirty = true;

        self.selection_paint.set_stroke_width(self.stroke_size());
        self.selection_style.set_font_size(self.font_size());
        self.text_style.set_font_size(self.font_size());
        self.fallback_metrics = None;
    }

    /// Set keyboard focus state.
    pub fn set_keyboard_focus(&mut self, focused: bool) {
        self.dirty |= self.keyboard_focused != focused;
        self.keyboard_focused = focused;
    }

    /// Set IME focus state.
    pub fn set_ime_focus(&mut self, focused: bool) {
        self.dirty |= self.ime_focused != focused;
        self.ime_focused = focused;
    }

    /// Handle config updates.
    pub fn update_config(&mut self, config: &Config) {
        // Check if any text field parameters changed.
        if self.font_size == config.font.size
            && self.font_family == config.font.family
            && self.paint.color4f() == config.colors.foreground.as_color4f()
        {
            return;
        }
        self.font_family = config.font.family.clone();
        self.font_size = config.font.size;
        self.fallback_metrics = None;
        self.dirty = true;

        // Update font options.

        self.paint.set_color4f(config.colors.foreground.as_color4f(), None);
        self.text_style.set_foreground_paint(&self.paint);
        self.text_style.set_font_size(self.font_size());
        self.text_style.set_font_families(&[&self.font_family]);

        self.selection_paint.set_color4f(config.colors.background.as_color4f(), None);
        self.selection_style.set_foreground_paint(&self.selection_paint);
        self.selection_paint.set_color4f(config.colors.highlight.as_color4f(), None);
        self.selection_style.set_background_paint(&self.selection_paint);
        self.selection_style.set_font_size(self.font_size());
        self.selection_style.set_font_families(&[&self.font_family]);
    }

    /// Replace the entire text box content.
    pub fn set_text(&mut self, text: String) {
        self.cursor_index = text.len();
        self.focus_cursor = true;
        self.text = text;

        self.clear_selection();

        self.text_input_dirty = true;
        self.dirty = true;
    }

    /// Handle new key press.
    pub fn press_key(&mut self, keysym: Keysym, modifiers: Modifiers) {
        // Ignore input with logo/alt key held.
        if modifiers.logo || modifiers.alt {
            return;
        }

        // Ensure cursor is visible after keyboard input.
        self.focus_cursor = true;

        match (keysym, modifiers.shift, modifiers.ctrl) {
            (Keysym::Left, false, false) => {
                self.cursor_index = match self.selection.take() {
                    Some(selection) => selection.start,
                    None => self.cursor_index.saturating_sub(1),
                };

                self.text_input_dirty = true;
                self.dirty = true;
            },
            (Keysym::Right, false, false) => {
                self.cursor_index = match self.selection.take() {
                    Some(selection) => selection.end,
                    None => cmp::min(self.cursor_index + 1, self.text.len()),
                };

                self.text_input_dirty = true;
                self.dirty = true;
            },
            (Keysym::BackSpace, false, false) => {
                if self.text.is_empty() {
                    return;
                }

                match self.selection.take() {
                    Some(selection) => self.delete_selected(selection),
                    None if self.cursor_index == 0 => return,
                    None => {
                        if self.text.is_empty() || self.cursor_index == 0 {
                            return;
                        }

                        // Jump to the previous character.
                        self.cursor_index = self.cursor_index.saturating_sub(1);
                        while self.cursor_index > 0
                            && !self.text.is_char_boundary(self.cursor_index)
                        {
                            self.cursor_index -= 1;
                        }

                        // Pop the character after the cursor.
                        self.text.remove(self.cursor_index);
                        self.persist_text();
                    },
                }

                self.text_input_dirty = true;
                self.dirty = true;
            },
            (Keysym::Delete, false, false) => {
                match self.selection.take() {
                    Some(selection) => self.delete_selected(selection),
                    None if self.cursor_index >= self.text.len() => return,
                    // Pop character after the cursor.
                    None => {
                        self.text.remove(self.cursor_index);
                        self.persist_text();
                    },
                }

                self.text_input_dirty = true;
                self.dirty = true;
            },
            (Keysym::Return, false, false) => {
                self.text.insert(self.cursor_index, '\n');
                self.persist_text();
                self.cursor_index += 1;

                self.text_input_dirty = true;
                self.dirty = true;
            },
            (Keysym::XF86_Copy, ..) | (Keysym::C, true, true) => {
                // Get selected text.
                let text = match self.selection_text() {
                    Some(text) => text.to_owned(),
                    None => return,
                };

                self.event_loop.insert_idle(move |state| {
                    let serial = state.clipboard.next_serial();
                    let copy_paste_source = state
                        .protocol_states
                        .data_device_manager
                        .create_copy_paste_source(&state.window.queue, ["text/plain"]);
                    copy_paste_source.set_selection(&state.protocol_states.data_device, serial);
                    state.clipboard.source = Some(copy_paste_source);
                    state.clipboard.text = text;
                });
            },
            (Keysym::XF86_Paste, ..) | (Keysym::V, true, true) => {
                self.event_loop.insert_idle(|state| {
                    // Get available Wayland text selection.
                    let selection_offer =
                        match state.protocol_states.data_device.data().selection_offer() {
                            Some(selection_offer) => selection_offer,
                            None => return,
                        };
                    let mut pipe = match selection_offer.receive("text/plain".into()) {
                        Ok(pipe) => pipe,
                        Err(err) => {
                            warn!("Clipboard paste failed: {err}");
                            return;
                        },
                    };

                    // Read text from pipe.
                    let mut text = String::new();
                    if let Err(err) = pipe.read_to_string(&mut text) {
                        error!("Failed to read from clipboard pipe: {err}");
                        return;
                    }

                    // Paste text into text box.
                    state.window.paste(&text);
                });
            },
            (keysym, _, false) => {
                let key_char = match keysym.key_char() {
                    Some(key_char) => key_char,
                    None => return,
                };

                // Delete selection before writing new text.
                if let Some(selection) = self.selection.take() {
                    self.delete_selected(selection);
                }

                // Add text at cursor position.
                self.text.insert(self.cursor_index, key_char);
                self.persist_text();

                // Move cursor behind inserted character.
                self.cursor_index += key_char.len_utf8();

                self.text_input_dirty = true;
                self.dirty = true;
            },
            _ => (),
        }
    }

    /// Handle touch press events.
    pub fn touch_down(&mut self, config: &Config, time: u32, mut position: Position<f64>) {
        // Adjust for text box being anchored to the bottom.
        position.y -= (self.size.height as f64 - self.last_paragraph_height as f64).max(0.);

        let offset = self.offset_at(position).unwrap_or(0);
        self.touch_state.down(config, time, position, offset);
    }

    /// Handle touch release.
    pub fn touch_motion(&mut self, config: &Config, mut position: Position<f64>) {
        // Adjust for text box being anchored to the bottom.
        position.y -= (self.size.height as f64 - self.last_paragraph_height as f64).max(0.);

        let delta = self.touch_state.motion(config, position, self.selection.as_ref());

        // Handle touch drag actions.
        match self.touch_state.action {
            TouchAction::Drag => {
                self.scroll_offset += delta.y as f32;

                self.text_input_dirty = true;
                self.dirty = true;
            },
            TouchAction::DragSelectionStart | TouchAction::DragSelectionEnd => {
                let offset = self.offset_at(position).unwrap_or(0);
                let selection = self.selection.as_mut().unwrap();

                // Update selection if it is at least one character wide.
                let modifies_start = self.touch_state.action == TouchAction::DragSelectionStart;
                if modifies_start && offset != selection.end {
                    selection.start = offset;
                } else if !modifies_start && offset != selection.start {
                    selection.end = offset;
                }

                // Swap modified side when input carets "overtake" each other.
                if selection.start > selection.end {
                    mem::swap(&mut selection.start, &mut selection.end);
                    self.touch_state.action = if modifies_start {
                        TouchAction::DragSelectionEnd
                    } else {
                        TouchAction::DragSelectionStart
                    };
                }

                // Ensure cursor is visible after selection change.
                self.focus_cursor = true;

                self.text_input_dirty = true;
                self.dirty = true;
            },
            // Ignore touch motion for tap actions.
            _ => (),
        }
    }

    /// Handle touch release.
    pub fn touch_up(&mut self) {
        // Ignore release handling for drag/focus actions.
        if matches!(
            self.touch_state.action,
            TouchAction::Drag | TouchAction::DragSelectionStart | TouchAction::DragSelectionEnd
        ) {
            return;
        }

        // Get byte offset from X/Y position.
        let position = self.touch_state.last_position;

        // Handle tap actions.
        match self.touch_state.action {
            TouchAction::Tap => {
                self.cursor_index = self.offset_at(position).unwrap_or(0);
                self.focus_cursor = true;

                self.clear_selection();

                self.text_input_dirty = true;
                self.dirty = true;
            },
            // Select word at touch position.
            TouchAction::DoubleTap => {
                let offset = self.offset_at(position).unwrap_or(0);

                let mut word_start = 0;
                let mut word_end = self.text.len();
                for (i, c) in self.text.char_indices() {
                    let c_end = i + c.len_utf8();
                    if c_end < offset && !c.is_alphanumeric() {
                        word_start = c_end;
                    } else if i > offset && !c.is_alphanumeric() {
                        word_end = i;
                        break;
                    }
                }

                self.select(word_start..word_end);
            },
            // Select everything.
            TouchAction::TripleTap => {
                let offset = self.offset_at(position).unwrap_or(0);
                let start = self.text[..offset].rfind('\n').map_or(0, |i| i + 1);
                let end = self.text[offset..].find('\n').map_or(self.text.len(), |i| offset + i);
                self.select(start..end);
            },
            TouchAction::Drag | TouchAction::DragSelectionStart | TouchAction::DragSelectionEnd => {
                unreachable!()
            },
        }
    }

    /// Paste text into the input element.
    pub fn paste(&mut self, text: &str) {
        // Delete selection before writing new text.
        if let Some(selection) = self.selection.take() {
            self.delete_selected(selection);
        }

        // Add text to input element.
        if self.cursor_index >= self.text.len() {
            self.text.push_str(text);
        } else {
            self.text.insert_str(self.cursor_index, text);
        }
        self.persist_text();

        // Move cursor behind the new characters.
        self.cursor_index += text.len();
        self.focus_cursor = true;

        self.text_input_dirty = true;
        self.dirty = true;
    }

    /// Delete text around the current cursor position.
    pub fn delete_surrounding_text(&mut self, before_length: u32, after_length: u32) {
        // Calculate removal boundaries.
        let end = (self.cursor_index + after_length as usize).min(self.text.len());
        let start = self.cursor_index.saturating_sub(before_length as usize);

        // Remove all bytes in the range from the text.
        self.text.truncate(end);
        self.text = self.text.split_off(start);
        self.persist_text();

        // Update cursor position.
        self.cursor_index = start;
        self.focus_cursor = true;

        self.text_input_dirty = true;
        self.dirty = true;
    }

    /// Insert text at the current cursor position.
    pub fn commit_string(&mut self, text: &str) {
        self.paste(text);
    }

    /// Set preedit text at the current cursor position.
    pub fn set_preedit_string(&mut self, text: String, _cursor_begin: i32, _cursor_end: i32) {
        // Delete selection as soon as preedit starts.
        if !text.is_empty()
            && let Some(selection) = self.selection.take()
        {
            self.delete_selected(selection);
        }

        self.preedit_text = text;
        self.focus_cursor = true;

        self.dirty = true;
    }

    /// Get physical dimensions of the last rendered cursor.
    pub fn last_cursor_rect(&self) -> Option<Rect> {
        self.last_cursor_rect
    }

    /// Modify text selection.
    fn select<R>(&mut self, range: R)
    where
        R: RangeBounds<usize>,
    {
        let mut start = match range.start_bound() {
            Bound::Included(start) => *start,
            Bound::Excluded(start) => *start + 1,
            Bound::Unbounded => usize::MIN,
        };
        start = start.max(0);
        let mut end = match range.end_bound() {
            Bound::Included(end) => *end + 1,
            Bound::Excluded(end) => *end,
            Bound::Unbounded => usize::MAX,
        };
        end = end.min(self.text.len());

        if start < end {
            self.selection = Some(start..end);

            // Ensure cursor is visible after selection change.
            self.focus_cursor = true;

            self.text_input_dirty = true;
            self.dirty = true;
        } else {
            self.clear_selection();
        }
    }

    /// Clear text selection.
    fn clear_selection(&mut self) {
        self.selection = None;

        self.text_input_dirty = true;
        self.dirty = true;
    }

    /// Get selection text.
    fn selection_text(&self) -> Option<&str> {
        let selection = self.selection.as_ref()?;
        Some(&self.text[selection.start..selection.end])
    }

    /// Delete the selected text.
    ///
    /// This automatically places the cursor at the start of the selection.
    fn delete_selected(&mut self, selection: Range<usize>) {
        // Remove selected text from input.
        self.text.drain(selection.start..selection.end);
        self.persist_text();

        // Update cursor.
        self.cursor_index = selection.start;
        self.focus_cursor = true;

        self.text_input_dirty = true;
        self.dirty = true;
    }

    /// Get byte index at the specified position.
    fn offset_at(&self, point: impl Into<Point>) -> Option<usize> {
        // Get position independent from current scroll offset.
        let mut point = point.into();
        point.y -= self.scroll_offset;

        // Get glyph cluster at the location.
        let paragraph = self.last_paragraph.as_ref()?;
        let cluster = paragraph.get_closest_glyph_cluster_at(point)?;

        // Calculate index based on position within the cluster.
        let width = cluster.bounds.right - cluster.bounds.left;
        let index = if point.x - cluster.bounds.left < width / 2. {
            cluster.text_range.start
        } else {
            cluster.text_range.end
        };

        Some(index.min(self.text.len()))
    }

    /// Get metrics for the glyph at the specified offset.
    fn metrics_at(&mut self, offset: usize) -> GlyphMetrics {
        match &self.last_paragraph {
            Some(paragraph) if offset > 0 => {
                let line_number = paragraph.get_line_number_at(offset - 1).unwrap_or(0);

                // Newlines are zerowidth glyphs at the end of the line, so we have to manually
                // move the cursor to the start of the following line.
                let (x, metrics) = if self.text.as_bytes()[offset - 1] == b'\n'
                    && let Some(metrics) = paragraph.get_line_metrics_at(line_number + 1)
                {
                    (0., metrics)
                } else {
                    let metrics = paragraph.get_line_metrics_at(line_number).unwrap();
                    let cluster = paragraph.get_glyph_cluster_at(offset - 1);
                    let x = cluster.map_or(0., |cluster| cluster.bounds.right);
                    (x, metrics)
                };

                GlyphMetrics::from_line_metrics(x, metrics)
            },
            Some(paragraph) => {
                let metrics = paragraph.get_line_metrics_at(0).unwrap();
                GlyphMetrics::from_line_metrics(0., metrics)
            },
            None => GlyphMetrics::from_font_metrics(0., self.fallback_metrics()),
        }
    }

    /// Get the caret's triangle points at the specified offset.
    fn caret_points(&mut self, offset: Point, index: usize) -> ([Point; 3], f32) {
        let caret_size = (CARET_SIZE * self.scale).round() as f32;
        let metrics = self.metrics_at(index);

        // Calculate width of the triangle outline at the tip.
        let stroke_point_width = SQRT_2 * self.stroke_size();

        let y = metrics.baseline - metrics.ascent - stroke_point_width / 2.;
        let line_height = metrics.ascent + metrics.descent;

        let points = [
            Point::new(offset.x + metrics.x - caret_size, offset.y + y - caret_size),
            Point::new(offset.x + metrics.x + caret_size, offset.y + y - caret_size),
            Point::new(offset.x + metrics.x, offset.y + y),
        ];

        (points, line_height)
    }

    /// Get surrounding text for IME.
    ///
    /// This will return at most `MAX_SURROUNDING_BYTES` bytes plus the current
    /// cursor positions relative to the surrounding text's origin.
    pub fn surrounding_text(&self) -> (String, i32, i32) {
        // Get up to half of `MAX_SURROUNDING_BYTES` after the cursor.
        let mut end = self.cursor_index + MAX_SURROUNDING_BYTES / 2;
        if end >= self.text.len() {
            end = self.text.len();
        } else {
            while end > 0 && !self.text.is_char_boundary(end) {
                end -= 1;
            }
        };

        // Get as many bytes as available before the cursor.
        let remaining = MAX_SURROUNDING_BYTES - (end - self.cursor_index);
        let mut start = self.cursor_index.saturating_sub(remaining);
        while start < self.text.len() && !self.text.is_char_boundary(start) {
            start += 1;
        }

        let (cursor_start, cursor_end) = match &self.selection {
            Some(selection) => (selection.start as i32, selection.end as i32),
            None => (self.cursor_index as i32, self.cursor_index as i32),
        };

        (self.text[start..end].into(), cursor_start - start as i32, cursor_end - start as i32)
    }

    /// Get font metrics for the fallback font.
    fn fallback_metrics(&mut self) -> FontMetrics {
        if self.fallback_metrics.is_none() {
            let fallback_typeface = self.font_collection.default_fallback().unwrap();
            let fallback_font = Font::new(fallback_typeface, self.font_size());
            self.fallback_metrics = Some(fallback_font.metrics().1);
        }
        *self.fallback_metrics.as_ref().unwrap()
    }

    /// Persist current text content to disk.
    ///
    /// This is automatically debounced to avoid excessive write operations.
    pub fn persist_text(&mut self) {
        // Debounce periods before text is persisted to disk.
        const MIN_DEBOUNCE: Duration = Duration::from_millis(1000);
        const MAX_DEBOUNCE: Duration = Duration::from_millis(5000);

        // Clear pending timers.
        if let Some(token) = self.persist_token.take() {
            self.event_loop.remove(token);
        }

        // Stage new persist timer, or write immediately if `MAX_DEBOUNCE` was reached.
        let start = self.persist_start.get_or_insert_with(Instant::now);
        let elapsed = start.elapsed();
        if elapsed >= MAX_DEBOUNCE {
            self.atomic_write();
            self.persist_start = None;
        } else {
            let debounce = cmp::min(MIN_DEBOUNCE, MAX_DEBOUNCE - elapsed);
            self.persist_token = self
                .event_loop
                .insert_source(Timer::from_duration(debounce), move |_, _, state| {
                    state.window.text_box.atomic_write();
                    TimeoutAction::Drop
                })
                .inspect_err(|err| error!("Failed to register write callback: {err}"))
                .ok();
        }
    }

    /// Attempt to atomically write a file.
    fn atomic_write(&mut self) {
        self.persist_start = None;

        // Get storage directory.
        let target_dir = match self.storage_path.parent() {
            Some(parent) => parent,
            None => {
                error!("Storage path cannot be filesystem root");
                return;
            },
        };

        // Ensure parent directory exists.
        if let Err(err) = fs::create_dir_all(target_dir) {
            error!("Could not create parent directories: {err}");
            return;
        }

        // Create a tempfile "next to" the target path.
        //
        // Creating this in the same directory as the target path should avoid errors
        // due to persisting across filesystems.
        let mut tempfile = match NamedTempFile::new_in(target_dir) {
            Ok(tempfile) => tempfile,
            Err(err) => {
                error!("Failed to create temporary file: {err}");
                return;
            },
        };

        // Write text with newline appended at the end.
        let result = tempfile.write_all(self.text.as_bytes()).and_then(|_| tempfile.write(b"\n"));
        if let Err(err) = result {
            error!("Failed to write to temporary file: {err}");
            return;
        }

        if let Err(err) = tempfile.persist(&self.storage_path) {
            error!("Failed move of temporary file: {err}");
        }

        info!("Successfully saved notes");
    }

    /// Monitor storage path for file changes.
    fn monitor_file(
        event_loop: &LoopHandle<'static, State>,
        storage_path: PathBuf,
    ) -> Result<(), Error> {
        let parent = match storage_path.parent() {
            Some(parent) => parent,
            None => {
                error!("Storage path cannot be filesystem root");
                return Ok(());
            },
        };

        // Create new monitor for the parent directory.
        let mut notify_source = NotifySource::new()?;
        notify_source.watch(parent, RecursiveMode::Recursive)?;

        // Watch for changes.
        event_loop.insert_source(notify_source, move |event, _, state| {
            // Ignore non-mutable events.
            if let EventKind::Access(_) = event.kind {
                return;
            }

            // Ignore other files in the storage directory.
            if !event.paths.contains(&storage_path) {
                return;
            }

            // Read file content.
            let content = match Self::read_to_string(&storage_path) {
                Some(content) => content,
                None => return,
            };

            // Update input if text changed.
            if state.window.text_box.text != content {
                info!("Reloading updated storage file");
                state.window.text_box.set_text(content);
                state.window.unstall();
            }
        })?;

        Ok(())
    }

    /// Read storage file to a string.
    ///
    /// This will return `None` if the file does not exist or access was denied.
    fn read_to_string(path: &PathBuf) -> Option<String> {
        // Read file content.
        let mut content = match fs::read_to_string(path) {
            Ok(content) => content,
            // Ignore file removal, since it might be done for replacement.
            Err(err) if err.kind() == IoErrorKind::NotFound => return None,
            Err(err) => {
                error!("Failed to read storage file at {path:?}: {err}");
                return None;
            },
        };

        // Strip trailing newline, commonly inserted by text editors.
        if content.ends_with('\n') {
            content.truncate(content.len() - 1);
        }

        Some(content)
    }

    /// Get the current font size.
    fn font_size(&self) -> f32 {
        (self.font_size * self.scale) as f32
    }

    /// Get the current caret stroke size.
    fn stroke_size(&self) -> f32 {
        (CARET_STROKE * self.scale) as f32
    }

    /// Update the scroll offset based on cursor position.
    ///
    /// This will scroll towards the cursor to ensure it is always visible.
    ///
    /// # Safety
    ///
    /// This updates the scroll offset based on the last paragraph, so calling
    /// it when `self.text` is changed from when `self.last_paragraph` was
    /// rendered will lead to invalid scroll offsets.
    unsafe fn update_scroll_offset(&mut self) {
        match self.selection.as_ref() {
            // For selections we jump twice, to make both ends visible if possible.
            Some(&Range { start, end }) => {
                unsafe { self.update_scroll_offset_to(start) };
                unsafe { self.update_scroll_offset_to(end) };
            },
            None => unsafe { self.update_scroll_offset_to(self.cursor_index) },
        }
    }

    /// Update the scroll offset to include a specific text byte offset.
    ///
    /// # Safety
    ///
    /// This updates the scroll offset based on the last paragraph, so calling
    /// it when `self.text` is changed from when `self.last_paragraph` was
    /// rendered will lead to invalid scroll offsets.
    unsafe fn update_scroll_offset_to(&mut self, offset: usize) {
        let metrics = self.metrics_at(offset);
        let line_end = metrics.baseline + metrics.descent;

        // Scroll cursor back into the visible range.
        let delta = line_end + self.scroll_offset - self.size.height as f32;
        if delta > 0. {
            self.scroll_offset -= delta;
        } else if line_end + self.scroll_offset < 0. {
            self.scroll_offset = -line_end;
        }

        unsafe { self.clamp_scroll_offset() };
    }

    /// Clamp the scroll offset to the text area's limits.
    ///
    /// # Safety
    ///
    /// This updates the scroll offset based on the last paragraph height, so
    /// calling it when `self.text` does not match the text used for calculating
    /// `self.last_paragraph_height` will lead to invalid scroll offsets.
    unsafe fn clamp_scroll_offset(&mut self) {
        let min_offset = -(self.last_paragraph_height - self.size.height as f32).max(0.);
        self.scroll_offset = self.scroll_offset.min(0.).max(min_offset);
    }
}

/// Touch event tracking.
#[derive(Default)]
struct TouchState {
    action: TouchAction,
    last_time: u32,
    last_position: Position<f64>,
    last_motion_position: Position<f64>,
    start_offset: usize,
}

impl TouchState {
    /// Update state from touch down event.
    fn down(&mut self, config: &Config, time: u32, position: Position<f64>, offset: usize) {
        // Update touch action.
        let delta = position - self.last_position;
        self.action = if self.last_time + config.input.max_multi_tap.as_millis() as u32 >= time
            && delta.x.powi(2) + delta.y.powi(2) <= config.input.max_tap_distance
        {
            match self.action {
                TouchAction::Tap => TouchAction::DoubleTap,
                TouchAction::DoubleTap => TouchAction::TripleTap,
                _ => TouchAction::Tap,
            }
        } else {
            TouchAction::Tap
        };

        // Reset touch origin state.
        self.last_motion_position = position;
        self.start_offset = offset;
        self.last_position = position;
        self.last_time = time;
    }

    /// Update state from touch motion event.
    ///
    /// Returns the distance moved since the last touch down or motion.
    fn motion(
        &mut self,
        config: &Config,
        position: Position<f64>,
        selection: Option<&Range<usize>>,
    ) -> Position<f64> {
        // Update incremental delta.
        let delta = position - self.last_motion_position;
        self.last_motion_position = position;

        // Never transfer out of drag/multi-tap states.
        if self.action != TouchAction::Tap {
            return delta;
        }

        // Ignore drags below the tap deadzone.
        let delta = position - self.last_position;
        if delta.x.powi(2) + delta.y.powi(2) <= config.input.max_tap_distance {
            return delta;
        }

        // Check if touch motion started on selection caret, with one character leeway.
        self.action = match selection {
            Some(selection) => {
                let start_delta = (self.start_offset as i32 - selection.start as i32).abs();
                let end_delta = (self.start_offset as i32 - selection.end as i32).abs();

                if end_delta <= start_delta && end_delta < 2 {
                    TouchAction::DragSelectionEnd
                } else if start_delta < 2 {
                    TouchAction::DragSelectionStart
                } else {
                    TouchAction::Drag
                }
            },
            _ => TouchAction::Drag,
        };

        delta
    }
}

/// Intention of a touch sequence.
#[derive(Default, PartialEq, Eq, Copy, Clone, Debug)]
enum TouchAction {
    #[default]
    Tap,
    DoubleTap,
    TripleTap,
    Drag,
    DragSelectionStart,
    DragSelectionEnd,
}

/// Glyph position metrics for a paragraph.
struct GlyphMetrics {
    /// Baseline position from the top of the paragraph.
    baseline: f32,
    /// Glyph descent.
    descent: f32,
    /// Glyph ascent.
    ascent: f32,
    /// X position from the left of the paragraph.
    x: f32,
}

impl GlyphMetrics {
    fn from_line_metrics(x: f32, metrics: LineMetrics<'_>) -> Self {
        Self {
            x,
            baseline: metrics.baseline as f32,
            descent: metrics.descent as f32,
            ascent: metrics.ascent as f32,
        }
    }

    fn from_font_metrics(x: f32, metrics: FontMetrics) -> Self {
        Self { x, baseline: -metrics.ascent, descent: metrics.descent, ascent: -metrics.ascent }
    }
}
