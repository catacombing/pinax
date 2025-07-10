//! Wayland window rendering.

use std::io::{ErrorKind as IoErrorKind, Read, Write};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::time::{Duration, Instant};
use std::{cmp, fs, mem};

use _text_input::zwp_text_input_v3::{ChangeCause, ContentHint, ContentPurpose, ZwpTextInputV3};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{LoopHandle, RegistrationToken};
use calloop_notify::NotifySource;
use calloop_notify::notify::{EventKind, RecursiveMode, Watcher};
use glutin::display::{Display, DisplayApiPreference};
use raw_window_handle::{RawDisplayHandle, WaylandDisplayHandle};
use skia_safe::textlayout::{
    Affinity, FontCollection, Paragraph, ParagraphBuilder, ParagraphStyle, PositionWithAffinity,
    TextDecoration, TextStyle,
};
use skia_safe::{Canvas as SkiaCanvas, Color4f, Font, FontMetrics, FontMgr, Paint, Point, Rect};
use smithay_client_toolkit::compositor::{CompositorState, Region};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::text_input::zv3::client as _text_input;
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewport::WpViewport;
use smithay_client_toolkit::seat::keyboard::{Keysym, Modifiers};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::window::{Window as XdgWindow, WindowDecorations};
use tempfile::NamedTempFile;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::geometry::{Position, Size};
use crate::renderer::Renderer;
use crate::skia::Canvas;
use crate::wayland::ProtocolStates;
use crate::{Error, State};

/// Padding around the text box at scale 1.
const PADDING: f64 = 15.;

/// Size of the bullet points at scale 1.
const BULLET_POINT_SIZE: f32 = 5.;

/// Horizontal padding reserved for bullet points at scale 1.
///
/// This includes the space of the bullet point itself, so it should always be
/// bigger than `BULLET_POINT_SIZE`.
///
/// It is not distributed around the bullet point evenly, but instead the bullet
/// point is aligned to the left. This helps with balancing this setting and
/// `PADDING`.
const BULLET_POINT_PADDING: f32 = f32::max(BULLET_POINT_SIZE, 15.);

/// Maximum number of surrounding bytes submitted to IME.
///
/// The value `4000` is chosen to match the maximum Wayland protocol message
/// size, a higher value will lead to errors.
const MAX_SURROUNDING_BYTES: usize = 4000;

/// Wayland window.
pub struct Window {
    pub initial_draw_done: bool,

    queue: QueueHandle<State>,
    connection: Connection,
    xdg_window: XdgWindow,
    viewport: WpViewport,
    renderer: Renderer,

    ime_cause: Option<ChangeCause>,
    text_input: Option<TextInput>,

    background: Color4f,
    text_box: TextBox,
    canvas: Canvas,

    stalled: bool,
    dirty: bool,
    size: Size,
    scale: f64,
}

impl Window {
    pub fn new(
        event_loop: LoopHandle<'static, State>,
        protocol_states: &ProtocolStates,
        connection: Connection,
        queue: QueueHandle<State>,
        config: &Config,
    ) -> Result<Self, Error> {
        // Get EGL display.
        let display = NonNull::new(connection.backend().display_ptr().cast()).unwrap();
        let wayland_display = WaylandDisplayHandle::new(display);
        let raw_display = RawDisplayHandle::Wayland(wayland_display);
        let egl_display = unsafe { Display::new(raw_display, DisplayApiPreference::Egl)? };

        // Create the XDG shell window.
        let surface = protocol_states.compositor.create_surface(&queue);
        let xdg_window = protocol_states.xdg_shell.create_window(
            surface.clone(),
            WindowDecorations::RequestClient,
            &queue,
        );
        xdg_window.set_title("Pinax");
        xdg_window.set_app_id("Pinax");
        xdg_window.commit();

        // Create OpenGL renderer.
        let renderer = Renderer::new(egl_display, surface.clone());

        // Create surface's Wayland global handles.
        if let Some(fractional_scale) = &protocol_states.fractional_scale {
            fractional_scale.fractional_scaling(&queue, &surface);
        }
        let viewport = protocol_states.viewporter.viewport(&queue, &surface);

        // Default to a reasonable default size,
        let size = Size { width: 360, height: 720 };

        Ok(Self {
            connection,
            xdg_window,
            viewport,
            renderer,
            queue,
            size,
            background: config.colors.background.as_color4f(),
            text_box: TextBox::new(event_loop, config)?,
            stalled: true,
            dirty: true,
            scale: 1.,
            initial_draw_done: Default::default(),
            text_input: Default::default(),
            ime_cause: Default::default(),
            canvas: Default::default(),
        })
    }

    /// Redraw the window.
    pub fn draw(&mut self) {
        // Stall rendering if nothing changed since last redraw.
        if !self.dirty() {
            self.stalled = true;
            return;
        }
        self.initial_draw_done = true;
        self.dirty = false;

        // Update IME state.
        if self.text_box.text_input_dirty {
            self.update_text_input();
        }

        // Update viewporter logical render size.
        //
        // NOTE: This must be done every time we draw with Sway; it is not
        // persisted when drawing with the same surface multiple times.
        self.viewport.set_destination(self.size.width as i32, self.size.height as i32);

        // Mark entire window as damaged.
        let wl_surface = self.xdg_window.wl_surface();
        wl_surface.damage(0, 0, self.size.width as i32, self.size.height as i32);

        // Update text box's physical dimensions.
        self.text_box.set_size(self.text_size());
        self.text_box.set_scale_factor(self.scale);
        let origin = self.text_origin();

        // Render the window content.
        let physical_size = self.size * self.scale;
        self.renderer.draw(physical_size, |renderer| {
            self.canvas.draw(renderer.skia_config(), physical_size, |canvas| {
                canvas.clear(self.background);
                self.text_box.draw(canvas, origin);
            });
        });

        // Request a new frame.
        wl_surface.frame(&self.queue, wl_surface.clone());

        // Apply surface changes.
        wl_surface.commit();
    }

    /// Unstall the renderer.
    ///
    /// This will render a new frame if there currently is no frame request
    /// pending.
    fn unstall(&mut self) {
        // Ignore if unstalled or request came from background engine.
        if !mem::take(&mut self.stalled) {
            return;
        }

        // Redraw immediately to unstall rendering.
        self.draw();
        let _ = self.connection.flush();
    }

    /// Update the window's logical size.
    pub fn set_size(&mut self, compositor: &CompositorState, size: Size) {
        if self.size == size {
            return;
        }

        self.size = size;
        self.dirty = true;

        // Update the window's opaque region.
        //
        // This is done here since it can only change on resize, but the commit happens
        // atomically on redraw.
        if let Ok(region) = Region::new(compositor) {
            region.add(0, 0, size.width as i32, size.height as i32);
            self.xdg_window.wl_surface().set_opaque_region(Some(region.wl_region()));
        }

        self.unstall();
    }

    /// Update the window's DPI factor.
    pub fn set_scale_factor(&mut self, scale: f64) {
        if self.scale == scale {
            return;
        }
        self.scale = scale;
        self.dirty = true;

        self.unstall();
    }

    /// Handle config updates.
    pub fn update_config(&mut self, config: &Config) {
        let background = config.colors.background.as_color4f();
        if self.background != background {
            self.background = background;
            self.dirty = true;
        }

        self.text_box.update_config(config);

        self.unstall();
    }

    /// Check whether UI needs redraw.
    pub fn dirty(&self) -> bool {
        self.dirty || self.text_box.dirty
    }

    /// Handle touch press.
    pub fn touch_down(&mut self, config: &Config, time: u32, position: Position<f64>) {
        self.ime_cause = Some(ChangeCause::Other);

        // Clamp padding touch to nearest text box position.
        let text_size = self.text_size();
        let mut physical_position = position * self.scale;
        physical_position -= self.text_origin();
        physical_position.x = physical_position.x.clamp(0., text_size.width as f64);
        physical_position.y = physical_position.y.clamp(0., text_size.height as f64);
        self.text_box.touch_down(config, time, physical_position);

        self.unstall();
    }

    /// Handle touch release.
    pub fn touch_motion(&mut self, config: &Config, position: Position<f64>) {
        self.ime_cause = Some(ChangeCause::Other);

        // Clamp padding touch to nearest text box position.
        let text_size = self.text_size();
        let mut physical_position = position * self.scale;
        physical_position -= self.text_origin();
        physical_position.x = physical_position.x.clamp(0., text_size.width as f64);
        physical_position.y = physical_position.y.clamp(0., text_size.height as f64);
        self.text_box.touch_motion(config, physical_position);

        self.unstall();
    }

    /// Handle touch release.
    pub fn touch_up(&mut self) {
        self.ime_cause = Some(ChangeCause::Other);
        self.text_box.touch_up();
        self.unstall();
    }

    /// Handle keyboard focus.
    pub fn keyboard_enter(&mut self) {
        self.text_box.set_keyboard_focus(true);
        self.unstall();
    }

    /// Handle keyboard focus loss.
    pub fn keyboard_leave(&mut self) {
        self.text_box.set_keyboard_focus(false);
        self.unstall();
    }

    /// Handle keyboard key press.
    pub fn press_key(&mut self, _raw: u32, keysym: Keysym, modifiers: Modifiers) {
        self.ime_cause = Some(ChangeCause::Other);
        self.text_box.press_key(keysym, modifiers);
        self.unstall();
    }

    /// Paste text into the window.
    fn paste(&mut self, text: &str) {
        self.text_box.paste(text);
        self.unstall();
    }

    /// Handle IME focus.
    pub fn text_input_enter(&mut self, text_input: ZwpTextInputV3) {
        self.text_input = Some(text_input.into());
        self.text_box.set_ime_focus(true);
        self.update_text_input();
        self.unstall();
    }

    /// Handle IME focus loss.
    pub fn text_input_leave(&mut self) {
        self.text_box.set_ime_focus(false);
        self.text_input = None;
        self.unstall();
    }

    /// Delete text around the current cursor position.
    pub fn delete_surrounding_text(&mut self, before_length: u32, after_length: u32) {
        self.text_box.delete_surrounding_text(before_length, after_length);
        self.unstall();
    }

    /// Insert text at the current cursor position.
    pub fn commit_string(&mut self, text: String) {
        self.text_box.commit_string(&text);
        self.unstall();
    }

    /// Set preedit text at the current cursor position.
    pub fn set_preedit_string(&mut self, text: String, cursor_begin: i32, cursor_end: i32) {
        self.text_box.set_preedit_string(text, cursor_begin, cursor_end);
        self.unstall();
    }

    /// Apply pending text input changes.
    fn update_text_input(&mut self) {
        let origin = self.text_origin();

        let text_input = match &mut self.text_input {
            Some(text_input) => text_input,
            None => return,
        };

        text_input.enable();

        let (text, cursor) = self.text_box.surrounding_text(MAX_SURROUNDING_BYTES);
        text_input.set_surrounding_text(text, cursor as i32, cursor as i32);

        let cause = self.ime_cause.take().unwrap_or(ChangeCause::InputMethod);
        text_input.set_text_change_cause(cause);

        let content_hint = ContentHint::Completion
            | ContentHint::Spellcheck
            | ContentHint::Multiline
            | ContentHint::AutoCapitalization;
        text_input.set_content_type(content_hint, ContentPurpose::Normal);

        // Update logical cursor rectangle.
        if let Some(rect) = self.text_box.last_cursor_rect {
            let scale = self.scale as f32;

            let x = ((origin.x as f32 + rect.left) / scale).round() as i32;
            let y = ((origin.y as f32 + rect.top) / scale).round() as i32;
            let width = ((rect.left - rect.right) / scale).round() as i32;
            let height = ((rect.bottom - rect.top) / scale).round() as i32;

            text_input.set_cursor_rectangle(x, y, width, height);
        }

        text_input.commit();
    }

    /// Origin point of the text box.
    fn text_origin(&self) -> Position<f64> {
        let padding = (PADDING * self.scale).round();
        let bullet_padding = (BULLET_POINT_PADDING as f64 * self.scale).round();
        Position::new(padding + bullet_padding, padding)
    }

    /// Size of the text box.
    fn text_size(&self) -> Size {
        let physical_size = self.size * self.scale;
        let padding = (PADDING * self.scale).round() as u32;
        let bullet_padding = (BULLET_POINT_PADDING as f64 * self.scale).round() as u32;
        physical_size - Size::new(padding * 2 + bullet_padding, padding * 2)
    }
}

/// An area for text input.
pub struct TextBox {
    event_loop: LoopHandle<'static, State>,

    fallback_metrics: Option<FontMetrics>,
    font_collection: FontCollection,
    text_style: TextStyle,
    paint: Paint,

    last_paragraph: Option<Paragraph>,
    last_paragraph_height: f32,
    last_cursor_rect: Option<Rect>,

    preedit_text: String,
    text: String,

    cursor_index: usize,

    size: Size,
    scale: f64,

    font_family: String,
    font_size: f64,

    touch_state: TouchState,

    keyboard_focused: bool,
    ime_focused: bool,

    persist_token: Option<RegistrationToken>,
    persist_start: Option<Instant>,
    storage_path: PathBuf,

    text_input_dirty: bool,
    dirty: bool,
}

impl TextBox {
    fn new(event_loop: LoopHandle<'static, State>, config: &Config) -> Result<Self, Error> {
        let font_family = config.font.family.clone();
        let font_size = config.font.size;

        let mut paint = Paint::default();
        paint.set_color4f(config.colors.foreground.as_color4f(), None);
        paint.set_anti_alias(true);

        let mut text_style = TextStyle::new();
        text_style.set_foreground_paint(&paint);
        text_style.set_font_size(font_size as f32);
        text_style.set_font_families(&[&font_family]);

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
            preedit_text: Default::default(),
            ime_focused: Default::default(),
            touch_state: Default::default(),
            size: Default::default(),
        })
    }

    /// Render text content to the canvas.
    fn draw(&mut self, canvas: &SkiaCanvas, point: impl Into<Point>) {
        let mut point = point.into();

        self.dirty = false;

        // Render text if not empty.
        self.last_paragraph = None;
        if !self.text.is_empty() || !self.preedit_text.is_empty() {
            // Shape text into paragraph.
            let mut paragraph_style = ParagraphStyle::new();
            paragraph_style.set_text_style(&self.text_style);
            let mut paragraph_builder =
                ParagraphBuilder::new(&paragraph_style, self.font_collection.clone());
            paragraph_builder.add_text(self.text.clone());

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
                paragraph_builder.add_text(self.preedit_text.clone());
            }

            // Build paragraph and calculate its height.
            let mut paragraph = paragraph_builder.build();
            paragraph.layout(self.size.width as f32);
            self.last_paragraph_height = paragraph.height();

            // Render text.
            point.y += self.size.height as f32 - self.last_paragraph_height;
            paragraph.paint(canvas, point);

            // Add bullet points in front of list elements.
            let bullet_lines = self
                .text
                .match_indices("\n\n")
                .map(|(i, _)| paragraph.get_line_number_at(i).unwrap() + 2);
            for line in bullet_lines.chain([0]) {
                let metrics = paragraph.get_line_metrics_at(line).unwrap();
                let size = BULLET_POINT_SIZE * self.scale as f32;
                let y = point.y + metrics.baseline as f32 - metrics.ascent as f32 / 2.
                    + metrics.descent as f32 / 2.
                    - size / 2.;
                let x = point.x - BULLET_POINT_PADDING * self.scale as f32;
                let rect = Rect::new(x, y, x + size, y + size);
                canvas.draw_rect(rect, &self.paint);
            }

            self.last_paragraph = Some(paragraph);
        } else {
            // Anchor content to the bottom of the window.
            let metrics = self.fallback_metrics();
            self.last_paragraph_height = metrics.descent - metrics.ascent;
            point.y += self.size.height as f32 - self.last_paragraph_height;

            // Handle bullet point drawing without any text.
            let size = BULLET_POINT_SIZE * self.scale as f32;
            let y = point.y - metrics.ascent / 2. + metrics.descent / 2. - size / 2.;
            let x = point.x - BULLET_POINT_PADDING * self.scale as f32;
            let rect = Rect::new(x, y, x + size, y + size);
            canvas.draw_rect(rect, &self.paint);
        }

        // Draw cursor while focused.
        self.last_cursor_rect = None;
        if self.keyboard_focused || self.ime_focused {
            // Get metrics at cursor position.
            let (x, baseline, ascent, descent) = match &self.last_paragraph {
                Some(paragraph) if self.cursor_index > 0 => {
                    let line_number = paragraph.get_line_number_at(self.cursor_index - 1).unwrap();

                    // Newlines are zerowidth glyphs at the end of the line, so we have to manually
                    // move the cursor to the start of the following line.
                    let (x, metrics) = if self.text.as_bytes()[self.cursor_index - 1] == b'\n' {
                        let metrics = paragraph.get_line_metrics_at(line_number + 1).unwrap();
                        (point.x, metrics)
                    } else {
                        let metrics = paragraph.get_line_metrics_at(line_number).unwrap();
                        let cluster =
                            paragraph.get_glyph_cluster_at(self.cursor_index - 1).unwrap();
                        (point.x + cluster.bounds.right, metrics)
                    };

                    (x, metrics.baseline, metrics.ascent as f32, metrics.descent as f32)
                },
                Some(paragraph) => {
                    let metrics = paragraph.get_line_metrics_at(0).unwrap();
                    (point.x, metrics.baseline, metrics.ascent as f32, metrics.descent as f32)
                },
                None => {
                    // Put cursor at the bottom of the screen.
                    let metrics = self.fallback_metrics();
                    (point.x, -metrics.ascent as f64, -metrics.ascent, metrics.descent)
                },
            };

            // Calculate cursor bounding box.
            let y = point.y + baseline as f32 - ascent;
            let width = self.scale.round() as f32;
            let height = (ascent + descent).round();

            // Render the cursor rectangle.
            let rect = Rect::new(x, y, x + width, y + height);
            canvas.draw_rect(rect, &self.paint);

            self.last_cursor_rect = Some(rect);
        }
    }

    /// Set the text box's physical size.
    fn set_size(&mut self, size: Size) {
        if self.size == size {
            return;
        }
        self.dirty = true;
        self.size = size;
    }

    /// Set the text box's font scale.
    fn set_scale_factor(&mut self, scale: f64) {
        if self.scale == scale {
            return;
        }
        self.scale = scale;
        self.dirty = true;

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
    fn update_config(&mut self, config: &Config) {
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
    }

    /// Get the current font size.
    fn font_size(&self) -> f32 {
        (self.font_size * self.scale) as f32
    }

    /// Handle new key press.
    fn press_key(&mut self, keysym: Keysym, modifiers: Modifiers) {
        // Ignore input with logo/alt key held.
        if modifiers.logo || modifiers.alt {
            return;
        }

        match (keysym, modifiers.shift, modifiers.ctrl) {
            (Keysym::Left, false, false) => {
                self.cursor_index = self.cursor_index.saturating_sub(1);
                self.text_input_dirty = true;
                self.dirty = true;
            },
            (Keysym::Right, false, false) => {
                self.cursor_index = cmp::min(self.cursor_index + 1, self.text.len());
                self.text_input_dirty = true;
                self.dirty = true;
            },
            (Keysym::BackSpace, false, false) => {
                if self.text.is_empty() {
                    return;
                }

                // Jump to the previous character.
                self.cursor_index = self.cursor_index.saturating_sub(1);
                while self.cursor_index > 0 && !self.text.is_char_boundary(self.cursor_index) {
                    self.cursor_index -= 1;
                }

                // Pop the character after the cursor.
                self.text.remove(self.cursor_index);
                self.persist_text();

                self.text_input_dirty = true;
                self.dirty = true;
            },
            (Keysym::Delete, false, false) => {
                if self.cursor_index == self.text.len() {
                    return;
                }

                // Pop character after the cursor.
                if self.cursor_index < self.text.len() {
                    self.text.remove(self.cursor_index);
                    self.persist_text();
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
                // We just copy all text since selection is not implemented yet.
                let text = self.text.clone();
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
                if let Some(key_char) = keysym.key_char() {
                    // Add text at cursor position.
                    self.text.insert(self.cursor_index, key_char);
                    self.persist_text();

                    // Move cursor behind inserted character.
                    self.cursor_index += key_char.len_utf8();

                    self.text_input_dirty = true;
                    self.dirty = true;
                }
            },
            _ => (),
        }
    }

    /// Handle touch press events.
    pub fn touch_down(&mut self, config: &Config, time: u32, mut position: Position<f64>) {
        // Adjust for text box being anchored to the bottom.
        position.y -= self.size.height as f64 - self.last_paragraph_height as f64;

        let offset = self.byte_index_at(position).unwrap_or(0);
        self.touch_state.down(config, time, position, offset);
    }

    /// Handle touch release.
    pub fn touch_motion(&mut self, config: &Config, mut position: Position<f64>) {
        // Adjust for text box being anchored to the bottom.
        position.y -= self.size.height as f64 - self.last_paragraph_height as f64;

        self.touch_state.motion(config, position);
    }

    /// Handle touch release.
    pub fn touch_up(&mut self) {
        // Ignore release handling for drag/focus actions.
        if matches!(self.touch_state.action, TouchAction::Drag) {
            return;
        }

        // Get byte offset from X/Y position.
        let position = self.touch_state.last_position;
        let offset = self.byte_index_at(position).unwrap_or(0);

        // Handle tap actions.
        if let TouchAction::Tap = self.touch_state.action {
            self.cursor_index = offset;

            self.text_input_dirty = true;
            self.dirty = true;
        }
    }

    /// Paste text into the input element.
    fn paste(&mut self, text: &str) {
        // Add text to input element.
        if self.cursor_index == self.text.len() {
            self.text.push_str(text);
        } else {
            self.text.insert_str(self.cursor_index, text);
        }
        self.persist_text();

        // Move cursor behind the new characters.
        self.cursor_index += text.len();

        self.text_input_dirty = true;
        self.dirty = true;
    }

    /// Delete text around the current cursor position.
    fn delete_surrounding_text(&mut self, before_length: u32, after_length: u32) {
        // Calculate removal boundaries.
        let end = (self.cursor_index + after_length as usize).min(self.text.len());
        let start = self.cursor_index.saturating_sub(before_length as usize);

        // Remove all bytes in the range from the text.
        self.text.truncate(end);
        self.text = self.text.split_off(start);
        self.persist_text();

        // Update cursor position.
        self.cursor_index = start;

        self.text_input_dirty = true;
        self.dirty = true;
    }

    /// Insert text at the current cursor position.
    fn commit_string(&mut self, text: &str) {
        self.paste(text);
    }

    /// Set preedit text at the current cursor position.
    pub fn set_preedit_string(&mut self, text: String, _cursor_begin: i32, _cursor_end: i32) {
        self.preedit_text = text;
        self.dirty = true;
    }

    /// Get byte offset at the specified position.
    fn byte_index_at(&self, point: impl Into<Point>) -> Option<usize> {
        let paragraph = self.last_paragraph.as_ref()?;
        let position = match paragraph.get_glyph_position_at_coordinate(point) {
            PositionWithAffinity { affinity: Affinity::Upstream, position } => position as usize,
            // With downstream affinity, pick the index after the glyph.
            PositionWithAffinity { affinity: Affinity::Downstream, position } => {
                let mut offset = position as usize;
                while offset < self.text.len() && !self.text.is_char_boundary(offset) {
                    offset += 1;
                }
                offset
            },
        };
        Some(position)
    }

    /// Get surrounding text for IME.
    ///
    /// This will return at most `max_len` bytes, while translating the
    /// initial cursor position to the new position relative
    /// to the surrounding text's start.
    fn surrounding_text(&self, max_len: usize) -> (String, usize) {
        // Get up to half of `max_len` after the cursor.
        let mut end = self.cursor_index + max_len / 2;
        if end >= self.text.len() {
            end = self.text.len();
        } else {
            while end > 0 && !self.text.is_char_boundary(end) {
                end -= 1;
            }
        };

        // Get as many bytes as available before the cursor.
        let remaining = max_len - (end - self.cursor_index);
        let mut start = self.cursor_index.saturating_sub(remaining);
        while start < self.text.len() && !self.text.is_char_boundary(start) {
            start += 1;
        }

        (self.text[start..end].into(), self.cursor_index - start)
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
    fn persist_text(&mut self) {
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

        if let Err(err) = tempfile.write_all(self.text.as_bytes()) {
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

                state.window.text_box.cursor_index = content.len();
                state.window.text_box.text = content;

                state.window.text_box.text_input_dirty = true;
                state.window.text_box.dirty = true;

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
}

/// Text input with enabled-state tracking.
#[derive(Debug)]
pub struct TextInput {
    text_input: ZwpTextInputV3,
    enabled: bool,
}

impl From<ZwpTextInputV3> for TextInput {
    fn from(text_input: ZwpTextInputV3) -> Self {
        Self { text_input, enabled: false }
    }
}

impl TextInput {
    /// Enable text input on a surface.
    ///
    /// This is automatically debounced if the text input is already enabled.
    ///
    /// Does not automatically send a commit, to allow synchronized
    /// initialization of all IME state.
    pub fn enable(&mut self) {
        if self.enabled {
            return;
        }

        self.enabled = true;
        self.text_input.enable();
    }

    /// Set the surrounding text.
    pub fn set_surrounding_text(&self, text: String, cursor_index: i32, selection_anchor: i32) {
        self.text_input.set_surrounding_text(text, cursor_index, selection_anchor);
    }

    /// Indicate the cause of surrounding text change.
    pub fn set_text_change_cause(&self, cause: ChangeCause) {
        self.text_input.set_text_change_cause(cause);
    }

    /// Set text field content purpose and hint.
    pub fn set_content_type(&self, hint: ContentHint, purpose: ContentPurpose) {
        self.text_input.set_content_type(hint, purpose);
    }

    /// Set text field cursor position.
    pub fn set_cursor_rectangle(&self, x: i32, y: i32, width: i32, height: i32) {
        self.text_input.set_cursor_rectangle(x, y, width, height);
    }

    /// Commit IME state.
    pub fn commit(&self) {
        self.text_input.commit();
    }
}

/// Touch event tracking.
#[derive(Default)]
struct TouchState {
    action: TouchAction,
    last_time: u32,
    last_position: Position<f64>,
    last_motion_position: Position<f64>,
    start_byte_index: usize,
}

impl TouchState {
    /// Update state from touch down event.
    fn down(&mut self, config: &Config, time: u32, position: Position<f64>, byte_index: usize) {
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
        self.start_byte_index = byte_index;
        self.last_motion_position = position;
        self.last_position = position;
        self.last_time = time;
    }

    /// Update state from touch motion event.
    ///
    /// Returns the distance moved since the last touch down or motion.
    fn motion(&mut self, config: &Config, position: Position<f64>) -> Position<f64> {
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

        self.action = TouchAction::Drag;

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
}
