//! Wayland window rendering.

use std::mem;
use std::ptr::NonNull;

use _text_input::zwp_text_input_v3::{ChangeCause, ContentHint, ContentPurpose, ZwpTextInputV3};
use calloop::LoopHandle;
use glutin::display::{Display, DisplayApiPreference};
use raw_window_handle::{RawDisplayHandle, WaylandDisplayHandle};
use skia_safe::Color4f;
use smithay_client_toolkit::compositor::{CompositorState, Region};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::text_input::zv3::client as _text_input;
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewport::WpViewport;
use smithay_client_toolkit::seat::keyboard::{Keysym, Modifiers};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::window::{Window as XdgWindow, WindowDecorations};

use crate::config::Config;
use crate::geometry::{Position, Size};
use crate::renderer::Renderer;
use crate::skia::Canvas;
use crate::text_box::TextBox;
use crate::wayland::ProtocolStates;
use crate::{Error, State};

/// Horizontal padding reserved for bullet points at scale 1.
///
/// This includes the space of the bullet point itself, so it should always be
/// bigger than `BULLET_POINT_SIZE`.
///
/// It is not distributed around the bullet point evenly, but instead the bullet
/// point is aligned to the left. This helps with balancing this setting and
/// `PADDING`.
pub const BULLET_POINT_PADDING: f32 = f32::max(BULLET_POINT_SIZE, 15.);

/// Size of the bullet points at scale 1.
pub const BULLET_POINT_SIZE: f32 = 5.;

/// Padding around the text box at scale 1.
const PADDING: f64 = 15.;

/// Wayland window.
pub struct Window {
    pub queue: QueueHandle<State>,
    pub initial_configure_done: bool,
    pub text_box: TextBox,

    connection: Connection,
    xdg_window: XdgWindow,
    viewport: WpViewport,
    renderer: Renderer,

    ime_cause: Option<ChangeCause>,
    text_input: Option<TextInput>,

    background: Color4f,
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

        // Create surface's Wayland global handles.
        let surface = protocol_states.compositor.create_surface(&queue);
        if let Some(fractional_scale) = &protocol_states.fractional_scale {
            fractional_scale.fractional_scaling(&queue, &surface);
        }
        let viewport = protocol_states.viewporter.viewport(&queue, &surface);

        // Create the XDG shell window.
        let xdg_window = protocol_states.xdg_shell.create_window(
            surface.clone(),
            WindowDecorations::RequestClient,
            &queue,
        );
        xdg_window.set_title("Pinax");
        xdg_window.set_app_id("Pinax");
        xdg_window.commit();

        // Create OpenGL renderer.
        let renderer = Renderer::new(egl_display, surface);

        // Default to a reasonable default size.
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
            initial_configure_done: Default::default(),
            text_input: Default::default(),
            ime_cause: Default::default(),
            canvas: Default::default(),
        })
    }

    /// Redraw the window.
    pub fn draw(&mut self) {
        // Stall rendering if nothing changed since last redraw.
        if !self.dirty() || !self.initial_configure_done {
            self.stalled = true;
            return;
        }
        self.dirty = false;

        // Update IME state.
        if self.text_box.take_text_input_dirty() {
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
    pub fn unstall(&mut self) {
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

        self.initial_configure_done = true;
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
        self.dirty || self.text_box.dirty()
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
    pub fn paste(&mut self, text: &str) {
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

    /// Persist current text content to disk.
    pub fn persist_text(&mut self) {
        self.text_box.persist_text();
    }

    /// Apply pending text input changes.
    fn update_text_input(&mut self) {
        let origin = self.text_origin();

        let text_input = match &mut self.text_input {
            Some(text_input) => text_input,
            None => return,
        };

        text_input.enable();

        let (text, cursor_start, cursor_end) = self.text_box.surrounding_text();
        text_input.set_surrounding_text(text, cursor_start, cursor_end);

        let cause = self.ime_cause.take().unwrap_or(ChangeCause::InputMethod);
        text_input.set_text_change_cause(cause);

        let content_hint = ContentHint::Completion
            | ContentHint::Spellcheck
            | ContentHint::Multiline
            | ContentHint::AutoCapitalization;
        text_input.set_content_type(content_hint, ContentPurpose::Normal);

        // Update logical cursor rectangle.
        if let Some(rect) = self.text_box.last_cursor_rect() {
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
