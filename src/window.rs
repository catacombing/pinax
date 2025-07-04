//! Wayland window rendering.

use std::ptr::NonNull;

use glutin::display::{Display, DisplayApiPreference};
use raw_window_handle::{RawDisplayHandle, WaylandDisplayHandle};
use smithay_client_toolkit::compositor::{CompositorState, Region};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewport::WpViewport;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{Anchor, Layer, LayerSurface};

use crate::geometry::Size;
use crate::renderer::Renderer;
use crate::wayland::ProtocolStates;
use crate::{Error, State, gl};

/// Wayland window.
pub struct Window {
    surface: LayerSurface,
    viewport: WpViewport,
    renderer: Renderer,

    size: Size,
    scale: f64,
}

impl Window {
    pub fn new(
        protocol_states: &ProtocolStates,
        connection: &Connection,
        queue: &QueueHandle<State>,
    ) -> Result<Self, Error> {
        // Get EGL display.
        let display = NonNull::new(connection.backend().display_ptr().cast()).unwrap();
        let wayland_display = WaylandDisplayHandle::new(display);
        let raw_display = RawDisplayHandle::Wayland(wayland_display);
        let egl_display = unsafe { Display::new(raw_display, DisplayApiPreference::Egl)? };

        // Create the layer shell window.
        let surface = protocol_states.compositor.create_surface(queue);
        let surface = protocol_states.layer_shell.create_layer_surface(
            queue,
            surface,
            Layer::Top,
            Some("notepad"),
            None,
        );
        surface.set_anchor(Anchor::LEFT | Anchor::TOP | Anchor::RIGHT | Anchor::BOTTOM);
        surface.set_exclusive_zone(-1);
        surface.set_size(0, 0);
        surface.commit();

        // Create OpenGL renderer.
        let wl_surface = surface.wl_surface();
        let renderer = Renderer::new(egl_display, wl_surface.clone());

        // Create surface's Wayland global handles.
        if let Some(fractional_scale) = &protocol_states.fractional_scale {
            fractional_scale.fractional_scaling(queue, wl_surface);
        }
        let viewport = protocol_states.viewporter.viewport(queue, wl_surface);

        Ok(Self { viewport, renderer, surface, scale: 1., size: Default::default() })
    }

    /// Redraw the window.
    pub fn draw(&mut self) {
        // Update viewporter logical render size.
        //
        // NOTE: This must be done every time we draw with Sway; it is not
        // persisted when drawing with the same surface multiple times.
        self.viewport.set_destination(self.size.width as i32, self.size.height as i32);

        // Mark entire window as damaged.
        let wl_surface = self.surface.wl_surface();
        wl_surface.damage(0, 0, self.size.width as i32, self.size.height as i32);

        // Render the window content.
        let physical_size = self.size * self.scale;
        self.renderer.draw(physical_size, |_| unsafe {
            gl::ClearColor(1., 0., 1., 1.);
            gl::Clear(gl::COLOR_BUFFER_BIT);
        });

        // Apply surface changes.
        wl_surface.commit();
    }

    /// Update the window's logical size.
    pub fn set_size(&mut self, compositor: &CompositorState, size: Size) {
        if self.size == size {
            return;
        }

        self.size = size;

        // Update the window's opaque region.
        //
        // This is done here since it can only change on resize, but the commit happens
        // atomically on redraw.
        if let Ok(region) = Region::new(compositor) {
            region.add(0, 0, size.width as i32, size.height as i32);
            self.surface.wl_surface().set_opaque_region(Some(region.wl_region()));
        }

        self.draw();
    }

    /// Update the window's DPI factor.
    pub fn set_scale_factor(&mut self, scale: f64) {
        if self.scale == scale {
            return;
        }

        self.scale = scale;

        self.draw();
    }
}
