//! Wayland bring-up and event dispatch: binding globals, creating the
//! window, and turning each incoming message into a change of state.

use crate::app::{
    State, ViewMode, APP_ID, FONT_SIZES, MAX_DIMENSION, MIN_HEIGHT, MIN_WIDTH, SCALE_120_UNITY,
    SERVER_ID_BASE,
};
use crate::files;
use crate::platform::error::{Error, Result};
use crate::platform::freetype::Fonts;
use crate::platform::protocol::{
    self, wl_callback, wl_compositor, wl_data_device_manager, wl_display, wl_output, wl_registry,
    wl_seat, wl_surface, wp_fractional_scale_manager_v1, wp_fractional_scale_v1, wp_viewporter,
    xdg_surface, xdg_toplevel, xdg_wm_base, zxdg_decoration_manager_v1,
    zxdg_toplevel_decoration_v1,
};
use crate::platform::wire::{Arg, Message, Reader};

impl State {
    pub(super) fn alloc_id(&mut self) -> u32 {
        self.free_ids.pop().unwrap_or_else(|| {
            let id = self.next_id;
            self.next_id += 1;
            id
        })
    }

    /// Walk from a bare connection to a configured, GPU-backed toplevel, open
    /// whatever the command line asked for, and run until the window closes.
    pub(super) fn bring_up(&mut self) -> Result<()> {
        self.registry = self.alloc_id();
        self.conn.request(
            protocol::WL_DISPLAY,
            wl_display::GET_REGISTRY,
            &[Arg::NewId(self.registry)],
        );
        self.roundtrip()?;

        let compositor = self
            .compositor
            .ok_or_else(|| Error::msg("compositor has no wl_compositor"))?;
        let wm_base = self
            .wm_base
            .ok_or_else(|| Error::msg("compositor has no xdg_wm_base (no xdg-shell?)"))?;

        // The GPU is mandatory: negotiate over dmabuf feedback and bring Vulkan
        // up, or exit saying so. The feedback must be committed before init_gpu
        // can pick a device, so wait out its burst first.
        self.request_dmabuf_feedback();
        self.roundtrip()?;
        self.init_gpu()
            .map_err(|e| Error::msg(format!("bnkfiles requires a Vulkan device: {e}")))?;

        // Surface stack: wl_surface -> xdg_surface -> xdg_toplevel.
        self.surface = self.alloc_id();
        self.conn.request(
            compositor,
            wl_compositor::CREATE_SURFACE,
            &[Arg::NewId(self.surface)],
        );
        self.xdg_surface = self.alloc_id();
        self.conn.request(
            wm_base,
            xdg_wm_base::GET_XDG_SURFACE,
            &[Arg::NewId(self.xdg_surface), Arg::Object(self.surface)],
        );
        self.toplevel = self.alloc_id();
        self.conn.request(
            self.xdg_surface,
            xdg_surface::GET_TOPLEVEL,
            &[Arg::NewId(self.toplevel)],
        );
        self.conn
            .request(self.toplevel, xdg_toplevel::SET_APP_ID, &[Arg::Str(APP_ID)]);
        self.conn.request(
            self.toplevel,
            xdg_toplevel::SET_MIN_SIZE,
            &[Arg::Int(MIN_WIDTH as i32), Arg::Int(MIN_HEIGHT as i32)],
        );
        self.refresh_title();

        // Ask for server-side decorations, so the window gets a titlebar where the
        // compositor draws them. Where it does not, the surface is borderless and
        // nothing else changes: we draw no decorations ourselves either way.
        if let Some(manager) = self.decoration_manager {
            self.decoration = self.alloc_id();
            self.conn.request(
                manager,
                zxdg_decoration_manager_v1::GET_TOPLEVEL_DECORATION,
                &[Arg::NewId(self.decoration), Arg::Object(self.toplevel)],
            );
            self.conn.request(
                self.decoration,
                zxdg_toplevel_decoration_v1::SET_MODE,
                &[Arg::Uint(protocol::decoration_mode::SERVER_SIDE)],
            );
        }

        // Fractional scaling: the per-surface scale object and the viewport that
        // maps the device-pixel buffer onto the logical window come as a pair.
        // Without both, the integer fallback carries it instead.
        if let (Some(fmgr), Some(vp)) = (self.scaling.fractional_manager, self.scaling.viewporter) {
            let fractional = self.alloc_id();
            self.conn.request(
                fmgr,
                wp_fractional_scale_manager_v1::GET_FRACTIONAL_SCALE,
                &[Arg::NewId(fractional), Arg::Object(self.surface)],
            );
            self.scaling.fractional_scale = fractional;
            let viewport = self.alloc_id();
            self.conn.request(
                vp,
                wp_viewporter::GET_VIEWPORT,
                &[Arg::NewId(viewport), Arg::Object(self.surface)],
            );
            self.scaling.viewport = viewport;
        }

        // The data device drives the clipboard; skip it where the compositor offers
        // no wl_data_device_manager (the viewer still works, just cannot copy).
        if let (Some(manager), Some(seat)) = (self.data_device_manager, self.seat) {
            let device = self.alloc_id();
            self.conn.request(
                manager,
                wl_data_device_manager::GET_DATA_DEVICE,
                &[Arg::NewId(device), Arg::Object(seat)],
            );
            self.data_device = device;
        }

        self.create_buffers()?;
        self.init_explicit_sync();

        // The empty commit draws the first configure out of the compositor; the
        // loop paints once `configured` is set.
        self.conn.request(self.surface, wl_surface::COMMIT, &[]);
        self.run_until(|s| s.configured)?;
        self.run_until(|s| s.closed)
    }

    /// The title: in the viewer, the picture's name and its place in the folder,
    /// both taken from the same index so a decode still in flight cannot pair an
    /// old name with a new count. In the explorer, the folder being shown.
    pub(super) fn refresh_title(&mut self) {
        if self.toplevel == 0 {
            return;
        }
        let title = match (self.mode, self.images.get(self.index)) {
            (ViewMode::Viewer, Some(path)) => format!(
                "{} ({}/{}) — bnkfiles",
                files::file_name(path),
                self.index + 1,
                self.images.len()
            ),
            _ => format!("{} — bnkfiles", files::file_name(&self.scan_dir)),
        };
        self.conn
            .request(self.toplevel, xdg_toplevel::SET_TITLE, &[Arg::Str(&title)]);
    }

    /// Ask the compositor to enter or leave fullscreen. The answer arrives as a
    /// configure carrying the new state, which is what `fullscreen` tracks.
    pub(super) fn toggle_fullscreen(&mut self) {
        let opcode = if self.fullscreen {
            xdg_toplevel::UNSET_FULLSCREEN
        } else {
            xdg_toplevel::SET_FULLSCREEN
        };
        // set_fullscreen takes an optional output; a null object means "you choose".
        let args: &[Arg] = if self.fullscreen {
            &[]
        } else {
            &[Arg::Object(0)]
        };
        self.conn.request(self.toplevel, opcode, args);
    }

    /// Bind a global at the version we know how to drive, never above what the
    /// compositor advertises.
    pub(super) fn bind_capped(
        &mut self,
        name: u32,
        interface: &str,
        version: u32,
        max: u32,
    ) -> u32 {
        let id = self.alloc_id();
        self.conn.request(
            self.registry,
            wl_registry::BIND,
            &[
                Arg::Uint(name),
                Arg::Bind {
                    interface,
                    version: version.min(max),
                    new_id: id,
                },
            ],
        );
        id
    }

    pub(super) fn on_global(&mut self, r: &mut Reader) -> Result<()> {
        let name = r.u32()?;
        let interface = r.string()?;
        let version = r.u32()?;
        match interface {
            protocol::IFACE_COMPOSITOR => {
                // Refused below version 4: bring-up then reports no compositor,
                // rather than damage being sent with a request that is not there.
                if version >= protocol::VERSION_COMPOSITOR {
                    let id =
                        self.bind_capped(name, interface, version, protocol::VERSION_COMPOSITOR);
                    self.compositor = Some(id);
                }
            }
            protocol::IFACE_WM_BASE => {
                let id = self.bind_capped(name, interface, version, protocol::VERSION_WM_BASE);
                self.wm_base = Some(id);
            }
            protocol::IFACE_SEAT => {
                let id = self.bind_capped(name, interface, version, protocol::VERSION_SEAT);
                self.seat = Some(id);
                self.seat_version = version.min(protocol::VERSION_SEAT);
            }
            protocol::IFACE_CURSOR_SHAPE_MANAGER => {
                let id = self.bind_capped(
                    name,
                    interface,
                    version,
                    protocol::VERSION_CURSOR_SHAPE_MANAGER,
                );
                self.cursor_shape_manager = Some(id);
            }
            protocol::IFACE_DECORATION_MANAGER => {
                let id = self.bind_capped(
                    name,
                    interface,
                    version,
                    protocol::VERSION_DECORATION_MANAGER,
                );
                self.decoration_manager = Some(id);
            }
            protocol::IFACE_DATA_DEVICE_MANAGER => {
                let id = self.bind_capped(
                    name,
                    interface,
                    version,
                    protocol::VERSION_DATA_DEVICE_MANAGER,
                );
                self.data_device_manager = Some(id);
            }
            protocol::IFACE_DMABUF if version >= protocol::VERSION_DMABUF => {
                let id = self.bind_capped(name, interface, version, protocol::VERSION_DMABUF);
                self.presentation.dmabuf = Some(id);
                self.presentation.dmabuf_version = version.min(protocol::VERSION_DMABUF);
            }
            protocol::IFACE_DRM_SYNCOBJ => {
                let id = self.bind_capped(name, interface, version, protocol::VERSION_DRM_SYNCOBJ);
                self.presentation.syncobj_manager = Some(id);
            }
            protocol::IFACE_FRACTIONAL_SCALE_MANAGER => {
                let id = self.bind_capped(
                    name,
                    interface,
                    version,
                    protocol::VERSION_FRACTIONAL_SCALE_MANAGER,
                );
                self.scaling.fractional_manager = Some(id);
            }
            protocol::IFACE_VIEWPORTER => {
                let id = self.bind_capped(name, interface, version, protocol::VERSION_VIEWPORTER);
                self.scaling.viewporter = Some(id);
            }
            protocol::IFACE_OUTPUT => {
                let id = self.bind_capped(name, interface, version, protocol::VERSION_OUTPUT);
                // Scale 1 until the output says otherwise; only the integer
                // fallback ever reads it.
                self.scaling.outputs.push((id, 1));
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn handle(&mut self, msg: Message) -> Result<()> {
        let mut r = Reader::new(&msg.body);

        if msg.object == protocol::WL_DISPLAY {
            match msg.opcode {
                wl_display::EV_ERROR => {
                    let object = r.u32()?;
                    let code = r.u32()?;
                    let message = r.string()?;
                    return Err(Error::msg(format!(
                        "wayland protocol error: object {object} code {code}: {message}"
                    )));
                }
                wl_display::EV_DELETE_ID => {
                    let id = r.u32()?;
                    if id < SERVER_ID_BASE {
                        self.free_ids.push(id);
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        if msg.object == self.registry && msg.opcode == wl_registry::EV_GLOBAL {
            return self.on_global(&mut r);
        }

        if Some(msg.object) == self.pending_sync && msg.opcode == wl_callback::EV_DONE {
            self.pending_sync = None;
            return Ok(());
        }

        // The frame callback fired: the compositor will take another frame.
        if self.frame_callback != 0
            && msg.object == self.frame_callback
            && msg.opcode == wl_callback::EV_DONE
        {
            self.frame_callback = 0;
            // A scrollbar mid-fade wants the frame the compositor has just offered.
            if self.explorer_bar.animating() {
                self.dirty = true;
            }
            return Ok(());
        }

        if msg.object == self.xdg_surface && msg.opcode == xdg_surface::EV_CONFIGURE {
            self.pending_configure = Some(r.u32()?);
            if let Some((lw, lh)) = self.pending_size.take() {
                // The configure size is logical; the buffer is device pixels.
                self.scaling.logical = (lw, lh);
                self.scaling.synced = false;
                let (dw, dh) = self.device_size(lw, lh);
                self.resize_to(dw, dh);
            }
            self.configured = true;
            self.dirty = true;
            return Ok(());
        }

        // The compositor's scale as 120ths: the primary source, which reopens the
        // fonts and resizes the buffer to exact device resolution.
        if self.scaling.fractional_scale != 0
            && msg.object == self.scaling.fractional_scale
            && msg.opcode == wp_fractional_scale_v1::EV_PREFERRED_SCALE
        {
            let factor_120 = r.u32()?;
            self.set_scale_120(factor_120);
            return Ok(());
        }

        // wl_surface.enter/leave and wl_output.scale drive the integer fallback.
        if msg.object == self.surface
            && (msg.opcode == wl_surface::EV_ENTER || msg.opcode == wl_surface::EV_LEAVE)
        {
            let output = r.u32()?;
            if msg.opcode == wl_surface::EV_ENTER {
                if !self.scaling.entered.contains(&output) {
                    self.scaling.entered.push(output);
                }
            } else {
                self.scaling.entered.retain(|&o| o != output);
            }
            self.refresh_integer_scale();
            return Ok(());
        }
        if msg.opcode == wl_output::EV_SCALE
            && self.scaling.outputs.iter().any(|(id, _)| *id == msg.object)
        {
            let factor = r.u32()? as i32;
            for out in &mut self.scaling.outputs {
                if out.0 == msg.object {
                    out.1 = factor.max(1);
                }
            }
            self.refresh_integer_scale();
            return Ok(());
        }

        if msg.object == self.toplevel {
            match msg.opcode {
                xdg_toplevel::EV_CONFIGURE => {
                    let w = r.u32()?;
                    let h = r.u32()?;
                    let states = r.array()?;
                    // Zero means "you choose"; keep the current size then.
                    if w > 0 && h > 0 {
                        self.pending_size = Some((w.min(MAX_DIMENSION), h.min(MAX_DIMENSION)));
                    }
                    // The compositor's word on whether we are fullscreen. Reading it
                    // here (rather than tracking what we last asked for) is what
                    // keeps the F key in step with a fullscreen entered from the
                    // compositor's own keybinding.
                    let was = self.fullscreen;
                    self.fullscreen = states
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|&state| u32::from_ne_bytes(state))
                        .any(|s| s == protocol::xdg_toplevel_state::FULLSCREEN);
                    if was != self.fullscreen {
                        self.dirty = true;
                    }
                }
                xdg_toplevel::EV_CLOSE => self.closed = true,
                _ => {}
            }
            return Ok(());
        }

        if Some(msg.object) == self.wm_base && msg.opcode == xdg_wm_base::EV_PING {
            let serial = r.u32()?;
            self.conn
                .request(msg.object, xdg_wm_base::PONG, &[Arg::Uint(serial)]);
            return Ok(());
        }

        if Some(msg.object) == self.seat && msg.opcode == wl_seat::EV_CAPABILITIES {
            let caps = r.u32()?;
            self.on_seat_capabilities(caps);
            return Ok(());
        }

        if self.keyboard != 0 && msg.object == self.keyboard {
            return self.on_keyboard(msg.opcode, &mut r);
        }

        if self.pointer != 0 && msg.object == self.pointer {
            return self.on_pointer(msg.opcode, &mut r);
        }

        // A clipboard source of ours: a paster asking for the bytes, or the
        // compositor telling us another selection has replaced ours.
        if self.clipboard.owns(msg.object) {
            return self.on_data_source(msg.object, msg.opcode, &mut r);
        }

        if self.data_device != 0 && msg.object == self.data_device {
            return self.on_data_device(msg.opcode, &mut r);
        }

        if self.presentation.feedback_id != 0 && msg.object == self.presentation.feedback_id {
            return self.on_dmabuf_feedback(msg.opcode, &mut r);
        }

        // A buffer the compositor is done reading: that slot is free to draw
        // into, or, for a buffer a resize replaced, its image can be freed.
        if self.on_buffer_event(msg.object, msg.opcode) {
            return Ok(());
        }

        Ok(())
    }

    /// Adopt a new scale: reopen the fonts at the device size the text is now drawn
    /// at, and resize the buffer to exact device resolution.
    pub(super) fn set_scale_120(&mut self, factor_120: u32) {
        let factor_120 = factor_120.max(1);
        if factor_120 == self.scaling.factor_120 {
            return;
        }
        self.scaling.factor_120 = factor_120;
        self.scaling.synced = false; // geometry + viewport/buffer-scale must be resent
        let sizes: Vec<u32> = FONT_SIZES.iter().map(|&px| self.font_px(px)).collect();
        match Fonts::with_selection(&self.font_selection, &sizes) {
            Ok(fonts) => {
                self.fonts = fonts;
                // The atlases hold glyphs rasterised at the old size.
                self.presentation.glyphs = crate::render::gpu::GlyphCache::new();
            }
            // Keep the old faces rather than die: text renders at the previous size
            // (a little soft), which beats no window at all.
            Err(e) => eprintln!("bnkfiles: could not reopen the fonts at scale {factor_120}: {e}"),
        }
        // The grid's cells just changed size with everything else, so the thumbnails
        // are now the wrong size. Nothing to clear: thumbnail keys carry their size
        // (see `thumb_key_of`), so the grid asks for new-size thumbnails under new keys,
        // the old-size ones already in flight complete under their own keys and are
        // evicted, and no stale-size result can overwrite a fresh one.
        let (lw, lh) = self.scaling.logical;
        let (dw, dh) = self.device_size(lw, lh);
        self.resize_to(dw, dh);
        self.dirty = true;
    }

    /// The integer fallback's scale, from the outputs the surface spans. A no-op
    /// once the fractional path is live.
    pub(super) fn refresh_integer_scale(&mut self) {
        if self.scaling.is_fractional() {
            return;
        }
        let factor_120 = self.scaling.integer_scale() as u32 * SCALE_120_UNITY;
        self.set_scale_120(factor_120);
    }
}
