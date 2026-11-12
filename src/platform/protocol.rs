//! Opcodes and constants for the Wayland interfaces the app uses, as the protocol
//! XML defines them: an opcode is the zero-based position of a request or event
//! in its interface. Only the requests the app sends and the events it handles are
//! listed. Request opcodes are bare names; event opcodes are prefixed `EV_`.

/// The `wl_display` object always has id 1.
pub const WL_DISPLAY: u32 = 1;

pub mod wl_display {
    pub const SYNC: u16 = 0;
    pub const GET_REGISTRY: u16 = 1;
    pub const EV_ERROR: u16 = 0;
    pub const EV_DELETE_ID: u16 = 1;
}

pub mod wl_registry {
    pub const BIND: u16 = 0;
    pub const EV_GLOBAL: u16 = 0;
}

pub mod wl_callback {
    pub const EV_DONE: u16 = 0;
}

pub mod wl_compositor {
    pub const CREATE_SURFACE: u16 = 0;
}

pub mod wl_surface {
    pub const ATTACH: u16 = 1;
    /// Request a one-shot `wl_callback` that fires `done` when the compositor is
    /// ready for the next frame, pacing redraws to the refresh rate.
    pub const FRAME: u16 = 3;
    pub const COMMIT: u16 = 6;
    /// Declare the integer scale the buffer is drawn at (the buffer is
    /// `logical * scale` pixels). The compositor divides by it to place the
    /// surface. Used on the integer-scale fallback path; the fractional path uses
    /// a viewport instead.
    pub const SET_BUFFER_SCALE: u16 = 8;
    /// Damage in buffer pixels (version 4), which is what the display list is
    /// measured in. Plain `damage` takes surface coordinates, which differ from
    /// them by the scale.
    pub const DAMAGE_BUFFER: u16 = 9;
    /// The surface is now shown on this `wl_output`; its argument is the output's
    /// object id. Tracked (with `leave`) to pick the surface's integer scale as
    /// the max over the outputs it spans.
    pub const EV_ENTER: u16 = 0;
    pub const EV_LEAVE: u16 = 1;
}

pub mod wl_output {
    /// The output's integer scale factor (`wl_output` version 2+). The fractional
    /// path ignores this; the integer fallback uses it.
    pub const EV_SCALE: u16 = 3;
}

// Fractional scaling (`fractional-scale-v1`, version 1). The manager mints a
// per-surface object that reports the compositor's preferred scale as a fixed
// 120ths value; combined with a viewport it lets a surface render at exact device
// resolution for scales like 1.25 or 1.5.
pub mod wp_fractional_scale_manager_v1 {
    /// Create a `wp_fractional_scale_v1` for a surface: `new_id`, then the surface.
    pub const GET_FRACTIONAL_SCALE: u16 = 1;
}

pub mod wp_fractional_scale_v1 {
    /// The preferred scale as `round(scale * 120)`: 120 is 1.0, 180 is 1.5.
    pub const EV_PREFERRED_SCALE: u16 = 0;
}

// Viewport (`viewporter`, version 1). A viewport maps a surface's (device-pixel)
// buffer onto a logical destination size, which is how the fractional path draws
// a `logical * scale` buffer yet presents at the logical window size.
pub mod wp_viewporter {
    /// Create a `wp_viewport` for a surface: `new_id`, then the surface.
    pub const GET_VIEWPORT: u16 = 1;
}

pub mod wp_viewport {
    /// The surface's logical size: `width`, `height` as integers (-1,-1 unsets).
    pub const SET_DESTINATION: u16 = 2;
}

pub mod wl_buffer {
    pub const DESTROY: u16 = 0;
    pub const EV_RELEASE: u16 = 0;
}

pub mod wl_seat {
    pub const GET_POINTER: u16 = 0;
    pub const GET_KEYBOARD: u16 = 1;
    pub const EV_CAPABILITIES: u16 = 0;
    /// The `pointer` bit in the capabilities bitfield.
    pub const CAP_POINTER: u32 = 1;
    /// The `keyboard` bit in the capabilities bitfield.
    pub const CAP_KEYBOARD: u32 = 2;
}

pub mod wl_pointer {
    /// Destroy the pointer (a destructor, since version 3): sent when the seat drops
    /// its pointer capability, so a re-added pointer starts from a fresh object.
    pub const RELEASE: u16 = 1;
    pub const EV_ENTER: u16 = 0;
    pub const EV_LEAVE: u16 = 1;
    pub const EV_MOTION: u16 = 2;
    pub const EV_BUTTON: u16 = 3;
    pub const EV_AXIS: u16 = 4;
    /// The end of one logical pointer event: the motion, button, and axis events
    /// before it belong together and are acted on here (v5+).
    pub const EV_FRAME: u16 = 5;
    /// What is doing the scrolling: a wheel's detents or a finger's smooth glide.
    pub const EV_AXIS_SOURCE: u16 = 6;
    /// A finger left the touchpad: the gesture is over, and the fraction of a
    /// notch it left behind belongs to it rather than to the next one.
    pub const EV_AXIS_STOP: u16 = 7;
    /// A wheel's detent count for this frame. A mouse wheel is quantised and this
    /// is what says so; the smooth `axis` value alone cannot tell a detent from a
    /// touchpad's glide.
    pub const EV_AXIS_DISCRETE: u16 = 8;
    /// The left mouse button (Linux input-event-codes `BTN_LEFT`).
    pub const BTN_LEFT: u32 = 0x110;
    /// wl_pointer.button_state: a press (vs 0 = release).
    pub const BUTTON_STATE_PRESSED: u32 = 1;
    /// wl_pointer.axis: the vertical scroll axis (vs 1 = horizontal).
    pub const AXIS_VERTICAL_SCROLL: u32 = 0;
    /// wl_pointer.axis_source: a touchpad finger, or a continuous device (a
    /// trackpoint). The wheel sources are told apart by their discrete steps.
    pub const AXIS_SOURCE_FINGER: u32 = 1;
    pub const AXIS_SOURCE_CONTINUOUS: u32 = 2;
}

pub mod wl_keyboard {
    /// Destroy the keyboard (a destructor, since version 3): sent when the seat drops
    /// its keyboard capability, so a re-added keyboard starts from a fresh object.
    pub const RELEASE: u16 = 0;
    pub const EV_KEYMAP: u16 = 0;
    pub const EV_ENTER: u16 = 1;
    pub const EV_LEAVE: u16 = 2;
    pub const EV_KEY: u16 = 3;
    pub const EV_MODIFIERS: u16 = 4;
    /// The auto-repeat rate (keys/sec) and delay (ms) the client must honour.
    pub const EV_REPEAT_INFO: u16 = 5;
    /// wl_keyboard.key_state: a press (vs 0 = release).
    pub const KEY_STATE_PRESSED: u32 = 1;
}

pub mod wl_data_device_manager {
    pub const CREATE_DATA_SOURCE: u16 = 0;
    pub const GET_DATA_DEVICE: u16 = 1;
}

pub mod wl_data_device {
    pub const SET_SELECTION: u16 = 1;
    /// The compositor introduces a new wl_data_offer (carries its new id).
    pub const EV_DATA_OFFER: u16 = 0;
}

pub mod wl_data_source {
    pub const OFFER: u16 = 0;
    pub const DESTROY: u16 = 1;
    /// A target pasted: write our data to the given fd, then close it.
    pub const EV_SEND: u16 = 1;
    /// Our source is no longer the selection; tear it down.
    pub const EV_CANCELLED: u16 = 2;
}

pub mod wl_data_offer {
    pub const DESTROY: u16 = 2;
}

pub mod xdg_wm_base {
    pub const GET_XDG_SURFACE: u16 = 2;
    pub const PONG: u16 = 3;
    pub const EV_PING: u16 = 0;
}

pub mod xdg_surface {
    pub const GET_TOPLEVEL: u16 = 1;
    pub const SET_WINDOW_GEOMETRY: u16 = 3;
    pub const ACK_CONFIGURE: u16 = 4;
    pub const EV_CONFIGURE: u16 = 0;
}

pub mod xdg_toplevel {
    pub const SET_TITLE: u16 = 2;
    pub const SET_APP_ID: u16 = 3;
    pub const SET_MIN_SIZE: u16 = 8;
    pub const SET_FULLSCREEN: u16 = 11;
    pub const UNSET_FULLSCREEN: u16 = 12;
    /// The compositor's chosen size and window states: `width`, `height`
    /// (ints), then a `states` array. A zero dimension means "you decide".
    pub const EV_CONFIGURE: u16 = 0;
    pub const EV_CLOSE: u16 = 1;
}

/// One of the `xdg_toplevel.configure` states, as they arrive in its `states`
/// array. The window's fullscreen/maximised state belongs to the compositor, so
/// this is the only honest source for it: a client-side toggle desyncs the moment
/// the user fullscreens the window with a compositor keybinding instead.
pub mod xdg_toplevel_state {
    pub const FULLSCREEN: u32 = 2;
}

// Server-side decorations (`xdg-decoration-unstable-v1`, version 1): ask the
// compositor to draw the titlebar. Where it refuses (or does not advertise the
// global at all) the surface is simply borderless, so this stays optional.
pub mod zxdg_decoration_manager_v1 {
    pub const GET_TOPLEVEL_DECORATION: u16 = 1;
}

pub mod zxdg_toplevel_decoration_v1 {
    pub const SET_MODE: u16 = 1;
}

/// `zxdg_toplevel_decoration_v1` modes.
pub mod decoration_mode {
    pub const SERVER_SIDE: u32 = 2;
}

pub mod zwp_linux_dmabuf_v1 {
    /// Create a params object to assemble one dmabuf-backed wl_buffer.
    pub const CREATE_PARAMS: u16 = 1;
    /// Create the feedback object announcing formats, modifiers, and devices
    /// for dmabuf presentation. Exists since version 4.
    pub const GET_DEFAULT_FEEDBACK: u16 = 2;
}

pub mod zwp_linux_buffer_params_v1 {
    pub const DESTROY: u16 = 0;
    /// One plane: an fd (ancillary), then `plane_idx`, `offset`, `stride`,
    /// `modifier_hi`, `modifier_lo`.
    pub const ADD: u16 = 1;
    /// Create the wl_buffer without waiting for a `created`/`failed` event;
    /// valid because the parameters come from the compositor's own feedback.
    /// Args: `new_id`, `width`, `height`, `format` (DRM fourcc), `flags`.
    pub const CREATE_IMMED: u16 = 3;
}

pub mod zwp_linux_dmabuf_feedback_v1 {
    pub const EV_DONE: u16 = 0;
    /// An fd (ancillary) holding the format+modifier table, plus its size.
    pub const EV_FORMAT_TABLE: u16 = 1;
    /// The compositor's main DRM device, as a dev_t in a wire array.
    pub const EV_MAIN_DEVICE: u16 = 2;
    pub const EV_TRANCHE_DONE: u16 = 3;
    pub const EV_TRANCHE_TARGET_DEVICE: u16 = 4;
    /// Native-endian u16 indices into the format table, as a wire array.
    pub const EV_TRANCHE_FORMATS: u16 = 5;
    pub const EV_TRANCHE_FLAGS: u16 = 6;
}

pub mod wp_cursor_shape_manager_v1 {
    /// Create a shape device for a `wl_pointer`: `new_id` then the pointer.
    pub const GET_POINTER: u16 = 1;
}

pub mod wp_cursor_shape_device_v1 {
    /// Destroy the device (a destructor): paired with the pointer, so it is torn down
    /// when the pointer is, and re-made with the pointer that replaces it.
    pub const DESTROY: u16 = 0;
    /// Set the pointer's shape: the `enter` event's `serial`, then a shape enum.
    pub const SET_SHAPE: u16 = 1;
    /// Shape enums from the protocol (both present since version 1): the plain arrow,
    /// and a pointing hand over a clickable control.
    pub const SHAPE_DEFAULT: u32 = 1;
    pub const SHAPE_POINTER: u32 = 4;
}

// Explicit sync (`linux-drm-syncobj-v1`, version 1). The client owns the
// timelines (one for acquire points, and a release timeline per swap buffer);
// the manager makes surface + timeline objects, and the surface carries the
// per-commit acquire/release points. No events.
pub mod wp_linux_drm_syncobj_manager_v1 {
    /// Attach explicit sync to a `wl_surface`: `new_id`, then the surface.
    pub const GET_SURFACE: u16 = 1;
    /// Import a DRM syncobj timeline: `new_id`, then the syncobj fd (ancillary).
    pub const IMPORT_TIMELINE: u16 = 2;
}

pub mod wp_linux_drm_syncobj_surface_v1 {
    /// The buffer in this commit must not be read until this timeline point
    /// signals: the `timeline` object, then the u64 point as `hi`, `lo`.
    pub const SET_ACQUIRE_POINT: u16 = 1;
    /// The compositor signals this point when done reading the buffer: the
    /// `timeline` object, then the u64 point as `hi`, `lo`.
    pub const SET_RELEASE_POINT: u16 = 2;
}

// Interface names, as advertised by `wl_registry.global`, and the maximum
// version we know how to drive.
pub const IFACE_COMPOSITOR: &str = "wl_compositor";
pub const IFACE_WM_BASE: &str = "xdg_wm_base";
pub const IFACE_SEAT: &str = "wl_seat";
pub const IFACE_DATA_DEVICE_MANAGER: &str = "wl_data_device_manager";
pub const IFACE_CURSOR_SHAPE_MANAGER: &str = "wp_cursor_shape_manager_v1";
pub const IFACE_DMABUF: &str = "zwp_linux_dmabuf_v1";
pub const IFACE_DRM_SYNCOBJ: &str = "wp_linux_drm_syncobj_manager_v1";
pub const IFACE_OUTPUT: &str = "wl_output";
pub const IFACE_FRACTIONAL_SCALE_MANAGER: &str = "wp_fractional_scale_manager_v1";
pub const IFACE_VIEWPORTER: &str = "wp_viewporter";
pub const IFACE_DECORATION_MANAGER: &str = "zxdg_decoration_manager_v1";

/// Also the least the app runs on: version 4 brought `damage_buffer`.
pub const VERSION_COMPOSITOR: u32 = 4;
pub const VERSION_WM_BASE: u32 = 1;
pub const VERSION_SEAT: u32 = 5;
pub const VERSION_DATA_DEVICE_MANAGER: u32 = 3;
/// The shapes used exist in version 1.
pub const VERSION_CURSOR_SHAPE_MANAGER: u32 = 1;
/// Version 4 introduced the feedback object (format table, main device,
/// tranches), which the GPU path requires; an older advertisement is not bound.
pub const VERSION_DMABUF: u32 = 4;
/// Explicit sync is a single version; we drive only what version 1 defines.
pub const VERSION_DRM_SYNCOBJ: u32 = 1;
/// Version 2 introduced the `scale` event, the only `wl_output` event the
/// integer-scale fallback needs; a v1-only output reports no scale (treated as 1).
pub const VERSION_OUTPUT: u32 = 2;
/// Both scale protocols are single-version.
pub const VERSION_FRACTIONAL_SCALE_MANAGER: u32 = 1;
pub const VERSION_VIEWPORTER: u32 = 1;
/// Decoration negotiation is a single version.
pub const VERSION_DECORATION_MANAGER: u32 = 1;
