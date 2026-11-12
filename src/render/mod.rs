//! The GPU presentation layer: everything about turning a frame's worth of
//! drawing commands into pixels on the compositor's surface.
//!
//! - `display`: the drawing vocabulary: `DrawCmd`, the `DisplayList` the app
//!   builds each frame, and the damage diff between frames.
//! - `gpu`: batching a `DisplayList` into vertices against the glyph atlases and
//!   image textures (the CPU half of a frame).
//! - `vulkan`: the device backend: bring-up, the exported dmabuf render
//!   targets, explicit sync, and the pipeline (the GPU half).
//! - `image`: decoding, and the store of decoded images the textures come from.
//!
//! It depends only on `platform` and itself, never on the app above it.

pub(crate) mod display;
pub(crate) mod gpu;
pub(crate) mod image;
pub(crate) mod vulkan;
