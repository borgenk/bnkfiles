//! The platform layer: the compositor, the keyboard, fonts, the filesystem and the
//! kernel, behind safe Rust interfaces.
//!
//! - **Wayland**: the wire codec (`wire`), the connection (`conn`), the protocol
//!   opcodes and constants (`protocol`), and dmabuf feedback (`dmabuf`).
//! - **Input**: keyboard translation via libxkbcommon (`xkb`).
//! - **Filesystem**: watching a directory for change (`inotify`).
//! - **Font**: FreeType rasterization (`freetype`), the colour-emoji face
//!   (`emoji`), HarfBuzz cluster shaping (`shape`), UAX #29 grapheme segmentation
//!   (`grapheme`), system font discovery (`fontconfig`), and ARGB pixel math
//!   (`pixel`).
//! - **Foundation**: the syscall FFI surface (`ffi`), the minimal error type
//!   (`error`), geometry (`geom`), and scroll math (`scroll`).
//!
//! A module here depends only on `std`, the system libraries and other `platform`
//! modules, never on the render or app layers above it, and holds only what the
//! app uses.

pub(crate) mod conn;
pub(crate) mod dmabuf;
pub(crate) mod emoji;
pub(crate) mod error;
pub(crate) mod ffi;
pub(crate) mod fontconfig;
pub(crate) mod freetype;
pub(crate) mod geom;
pub(crate) mod grapheme;
pub(crate) mod inotify;
pub(crate) mod pixel;
pub(crate) mod protocol;
pub(crate) mod scroll;
pub(crate) mod shape;
pub(crate) mod wire;
pub(crate) mod xkb;
