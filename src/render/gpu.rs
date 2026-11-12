//! The GPU backend's CPU half: turning a display list into GPU-consumable
//! data. This module is deliberately free of Vulkan: it packs glyph rasters
//! into atlas mirrors and emits vertices and draw batches as plain values, so
//! the geometry and packing logic is unit-testable without a device.
//! [`crate::render::vulkan`] consumes the output ([`FrameData`]) verbatim.
//!
//! ```text
//!   &[DrawCmd] ─┬─ Fill/RoundRect ──────────────▶ quads (solid / SDF corners)
//!               ├─ Text ─▶ FreeType rasters ─▶ atlas slots ─▶ textured quads
//!               └─ Image ─▶ the image store ──▶ per-image texture batches
//! ```
//!
//! Text traversal matches how layout measured each run (the ASCII fast path,
//! then grapheme clusters routed to the emoji or per-character path), so the
//! pixels the GPU composites land exactly where the caret and wrapping expect.

use std::collections::{HashMap, HashSet};

use crate::platform::freetype::{FaceKey, Fonts};
use crate::platform::geom::Rect;
use crate::platform::grapheme;
use crate::render::display::DrawCmd;
use crate::render::display::RoundedCorners;
use crate::render::image::{ImageKey, ImageStore};

/// Vertex modes, matching shaders/quad.frag.
pub const MODE_SOLID: u32 = 0;
pub const MODE_GLYPH: u32 = 1;
pub const MODE_EMOJI: u32 = 2;
pub const MODE_IMAGE: u32 = 3;
pub const MODE_ROUND: u32 = 4;

/// Atlases start here and double when full, up to [`ATLAS_MAX`] or the device's
/// largest image if that is smaller ([`GlyphCache::limit_atlases`]); growth
/// wipes the atlas (sources are cached in `Fonts`, so re-inserting is cheap) and
/// the frame build restarts so no stale coordinates survive.
const ATLAS_START: u32 = 1024;
const ATLAS_MAX: u32 = 8192;

/// One vertex, laid out exactly as the pipeline's vertex input expects
/// (`repr(C)`: pos, uv, color, extra, mode). Six of these make a quad.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vertex {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
    pub color: [f32; 4],
    pub extra: [f32; 4],
    pub mode: u32,
}

/// A contiguous vertex range drawn with one descriptor binding: `image` names
/// the texture for [`MODE_IMAGE`] quads, `None` for everything else (the
/// atlases are always bound).
#[derive(Debug, PartialEq, Eq)]
pub struct Batch {
    pub start: u32,
    pub count: u32,
    pub image: Option<TextureKey>,
}

/// Identifies one uploaded image texture: the image's key in the
/// [`ImageStore`] and the generation of the pixels it was uploaded from.
///
/// Zooming or resizing the window reuses the texture, since neither changes the
/// decode. A new decode under the same key (a file rewritten in place) has a new
/// generation, so it gets a new texture and the old one is left to be evicted.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct TextureKey {
    pub image: ImageKey,
    pub generation: u64,
}

/// A rectangle of an atlas that changed and needs a texture upload, with its
/// bytes extracted row-contiguous from the mirror.
pub struct AtlasUpload {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub bytes: Vec<u8>,
}

/// Everything the Vulkan renderer needs to draw one frame.
pub struct FrameData {
    pub vertices: Vec<Vertex>,
    pub batches: Vec<Batch>,
    /// Atlas geometry and generation, so the renderer can (re)create textures.
    pub glyph_atlas: AtlasInfo,
    pub emoji_atlas: AtlasInfo,
    /// Dirty-region uploads for the two atlases, if any.
    pub glyph_upload: Option<AtlasUpload>,
    pub emoji_upload: Option<AtlasUpload>,
    /// Image textures the renderer does not have yet, named by key only. The
    /// pixels stay in the store and are read straight into the mapped staging
    /// buffer at upload time, so the frame path never copies an image.
    pub image_uploads: Vec<TextureKey>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtlasInfo {
    pub width: u32,
    pub height: u32,
    /// Bumped whenever the atlas is wiped and regrown; the renderer recreates
    /// the texture (and re-uploads in full) when it changes.
    pub generation: u32,
}

/// Where a raster landed in an atlas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slot {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// A glyph's atlas placement plus its pen metrics, cached beside the slot so a
/// steady-state frame emits the quad without re-rasterising the glyph. `slot` is
/// `None` for an inkless glyph (a space, or an emoji that shaped to no ink) or
/// one the atlas could not fit; the pen still advances by `advance`, so the
/// caret and the glyphs after it land where layout measured.
#[derive(Clone, Copy)]
struct PackedGlyph {
    slot: Option<Slot>,
    left: i32,
    top: i32,
    advance: f32,
}

/// A shelf-packing texture atlas with a CPU mirror. Rasters are packed onto
/// horizontal shelves (rows share a height class); the mirror holds the texel
/// bytes so the GPU texture can be (re)filled at any time, and a dirty
/// rectangle accumulates what changed since the last upload.
pub struct Atlas {
    width: u32,
    height: u32,
    /// The side the atlas may grow to: [`ATLAS_MAX`], or less on a device whose
    /// largest texture is smaller.
    max: u32,
    /// Bytes per texel: 1 (R8 coverage) or 4 (BGRA emoji).
    bpp: u32,
    pixels: Vec<u8>,
    /// Shelves as (y, height, x_used).
    shelves: Vec<(u32, u32, u32)>,
    next_y: u32,
    generation: u32,
    /// Union of slots written since the last [`Atlas::take_upload`].
    dirty: Option<(u32, u32, u32, u32)>,
}

impl Atlas {
    pub fn new(bpp: u32) -> Self {
        Self {
            width: ATLAS_START,
            height: ATLAS_START,
            max: ATLAS_MAX,
            bpp,
            pixels: vec![0; (ATLAS_START * ATLAS_START * bpp) as usize],
            shelves: Vec::new(),
            next_y: 0,
            generation: 0,
            dirty: None,
        }
    }

    pub fn info(&self) -> AtlasInfo {
        AtlasInfo {
            width: self.width,
            height: self.height,
            generation: self.generation,
        }
    }

    /// Reserve a `w` x `h` slot. `None` means the atlas is full even after
    /// growing to its cap (the caller skips the raster; a pathological case).
    /// Growing wipes all previous slots and bumps the generation, which the
    /// frame builder answers by restarting the build.
    fn reserve(&mut self, w: u32, h: u32) -> Option<Slot> {
        if w == 0 || h == 0 || w > self.max || h > self.max {
            return None;
        }
        loop {
            // A shelf fits if the raster is no taller than the shelf and the
            // remaining width holds it. Shelves are created at the raster's
            // height, so height classes cluster and waste stays bounded.
            if let Some(shelf) = self
                .shelves
                .iter_mut()
                .find(|(_, sh, used)| h <= *sh && used + w <= self.width)
            {
                let slot = Slot {
                    x: shelf.2,
                    y: shelf.0,
                    w,
                    h,
                };
                shelf.2 += w;
                return Some(slot);
            }
            if self.next_y + h <= self.height && w <= self.width {
                self.shelves.push((self.next_y, h, w));
                let slot = Slot {
                    x: 0,
                    y: self.next_y,
                    w,
                    h,
                };
                self.next_y += h;
                return Some(slot);
            }
            if !self.grow() {
                return None;
            }
        }
    }

    /// Double the atlas (square), wiping every slot. Returns false at the cap.
    fn grow(&mut self) -> bool {
        let next = (self.width * 2).min(self.max);
        if next <= self.width {
            return false;
        }
        self.width = next;
        self.height = next;
        self.pixels = vec![0; (next * next * self.bpp) as usize];
        self.shelves.clear();
        self.next_y = 0;
        self.generation += 1;
        self.dirty = None;
        true
    }

    /// Empty the atlas at its current size and bump the generation, which kills
    /// every slot handed out before and has the renderer refill the texture.
    fn reclaim(&mut self) {
        self.pixels.fill(0);
        self.shelves.clear();
        self.next_y = 0;
        self.generation += 1;
        self.dirty = None;
    }

    /// Write a raster's rows into the mirror at `slot` and extend the dirty
    /// rectangle over it. `src` is `w * h * bpp` bytes, row-major, tight.
    fn write(&mut self, slot: Slot, src: &[u8]) {
        let row_bytes = (slot.w * self.bpp) as usize;
        for row in 0..slot.h {
            let src_start = row as usize * row_bytes;
            let dst_start = (((slot.y + row) * self.width + slot.x) * self.bpp) as usize;
            self.pixels[dst_start..dst_start + row_bytes]
                .copy_from_slice(&src[src_start..src_start + row_bytes]);
        }
        let rect = (slot.x, slot.y, slot.x + slot.w, slot.y + slot.h);
        self.dirty = Some(match self.dirty {
            None => rect,
            Some((x0, y0, x1, y1)) => (
                x0.min(rect.0),
                y0.min(rect.1),
                x1.max(rect.2),
                y1.max(rect.3),
            ),
        });
    }

    /// Reserve a slot for a `w` x `h` raster and write `src` into it, returning
    /// the slot; `None` for a zero-area raster or a full atlas (the caller
    /// records the empty placement so the pen still advances). `src` is
    /// `w * h * bpp` bytes, row-major, tight.
    fn pack(&mut self, w: u32, h: u32, src: &[u8]) -> Option<Slot> {
        // A raster shorter than its size claims is refused rather than read past.
        if w == 0 || h == 0 || src.len() < (w * h * self.bpp) as usize {
            return None;
        }
        let slot = self.reserve(w, h)?;
        self.write(slot, src);
        Some(slot)
    }

    /// The bytes of the dirty region (row-contiguous), clearing the flag.
    /// A texture recreated after a grow needs no separate full upload: the
    /// grow wiped the mirror, so everything in it is inside the dirty region.
    pub fn take_upload(&mut self) -> Option<AtlasUpload> {
        let (x0, y0, x1, y1) = self.dirty.take()?;
        let (w, h) = (x1 - x0, y1 - y0);
        let row_bytes = (w * self.bpp) as usize;
        let mut bytes = Vec::with_capacity(row_bytes * h as usize);
        for row in y0..y1 {
            let start = ((row * self.width + x0) * self.bpp) as usize;
            bytes.extend_from_slice(&self.pixels[start..start + row_bytes]);
        }
        Some(AtlasUpload {
            x: x0,
            y: y0,
            w,
            h,
            bytes,
        })
    }
}

/// Glyph-cache lookups since the cache was made: a hit drew from the atlas, a miss
/// rasterized. Instrumentation for the frame stats line.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
}

impl CacheStats {
    pub fn lookups(self) -> u64 {
        self.hits + self.misses
    }

    /// Hits over lookups, or zero before the first lookup.
    pub fn hit_rate(self) -> f32 {
        match self.lookups() {
            0 => 0.0,
            lookups => self.hits as f32 / lookups as f32,
        }
    }
}

/// How many placements each slot map holds before it is dropped whole. The keys
/// come from file names, and a recorded miss (an inkless glyph, a cluster that is
/// not emoji) occupies no atlas space, so nothing else would ever bound them.
/// Dropping a map costs a re-rasterization of what is on screen, into an atlas
/// that is reclaimed with it.
const MAX_SLOTS: usize = 16 * 1024;

/// The persistent glyph caches the batcher packs into: slots by scalar and by
/// cluster, over the two atlases. Owned by the app next to the display list;
/// survives across frames so steady-state frames insert nothing.
pub struct GlyphCache {
    pub glyphs: Atlas,
    pub emoji: Atlas,
    pub stats: CacheStats,
    scalar_slots: HashMap<(FaceKey, char), PackedGlyph>,
    /// Nested so lookups borrow the cluster as `&str` (no per-frame `String`).
    /// A `None` value records a cluster that is not colour emoji, so a
    /// steady-state frame takes the per-character path without re-consulting
    /// the `Face`.
    cluster_slots: HashMap<FaceKey, HashMap<String, Option<PackedGlyph>>>,
    /// Entries across every inner map of `cluster_slots`.
    clusters: usize,
    /// A slot map was already dropped at its cap during this frame build. A second
    /// drop would restart the build again for the same glyphs, forever, if one frame
    /// ever held more than [`MAX_SLOTS`]; the map runs over for that frame instead.
    reclaimed: bool,
}

impl GlyphCache {
    pub fn new() -> Self {
        Self {
            glyphs: Atlas::new(1),
            emoji: Atlas::new(4),
            stats: CacheStats::default(),
            scalar_slots: HashMap::new(),
            cluster_slots: HashMap::new(),
            clusters: 0,
            reclaimed: false,
        }
    }

    /// Keep both atlases within `max_dimension` pixels a side, the largest
    /// texture the device can create: an atlas grown past it could not be
    /// uploaded at all. The atlases never shrink below their starting size,
    /// which every Vulkan device can hold.
    pub fn limit_atlases(&mut self, max_dimension: u32) {
        let max = max_dimension.clamp(ATLAS_START, ATLAS_MAX);
        self.glyphs.max = max;
        self.emoji.max = max;
    }

    /// Record a scalar's placement. At the cap the glyph atlas is reclaimed with
    /// the records that pointed into it, which restarts the frame build.
    fn remember_scalar(&mut self, key: (FaceKey, char), packed: PackedGlyph) {
        if self.scalar_slots.len() >= MAX_SLOTS && !self.reclaimed {
            self.reclaimed = true;
            self.glyphs.reclaim();
            self.scalar_slots.clear();
        }
        self.scalar_slots.insert(key, packed);
    }

    /// Record a cluster's placement, or that it is not emoji. Capped like the
    /// scalars, over the colour atlas.
    fn remember_cluster(&mut self, key: FaceKey, cluster: &str, packed: Option<PackedGlyph>) {
        if self.clusters >= MAX_SLOTS && !self.reclaimed {
            self.reclaimed = true;
            self.emoji.reclaim();
            self.cluster_slots.clear();
            self.clusters = 0;
        }
        self.cluster_slots
            .entry(key)
            .or_default()
            .insert(cluster.to_string(), packed);
        self.clusters += 1;
    }

    /// Drop the slot records of an atlas that was wiped since the generations
    /// given.
    fn reset_for(&mut self, glyphs_gen: u32, emoji_gen: u32) {
        if self.glyphs.generation != glyphs_gen {
            self.scalar_slots.clear();
        }
        if self.emoji.generation != emoji_gen {
            self.cluster_slots.clear();
            self.clusters = 0;
        }
    }
}

/// Build one frame's GPU data from the display list. `known_images` is the
/// set of image textures the renderer already holds; missing ones are named in
/// the result for it to upload. Restarts internally if an atlas grows
/// mid-build, so the returned vertices always reference current atlas
/// coordinates.
pub fn build_frame(
    fonts: &Fonts,
    store: &ImageStore,
    list: &[DrawCmd],
    cache: &mut GlyphCache,
    known_images: &dyn Fn(&TextureKey) -> bool,
) -> FrameData {
    cache.reclaimed = false;
    loop {
        let glyphs_gen = cache.glyphs.generation;
        let emoji_gen = cache.emoji.generation;
        let mut b = Batcher {
            fonts,
            store,
            cache,
            vertices: Vec::new(),
            batches: Vec::new(),
            image_uploads: Vec::new(),
            upload_queued: HashSet::new(),
            known_images,
        };
        for cmd in list {
            b.command(cmd);
        }
        let (vertices, batches, image_uploads) = (b.vertices, b.batches, b.image_uploads);
        // An atlas grew (and wiped) during the build: earlier quads reference
        // dead coordinates. Clear the stale slot records and rebuild; the
        // second pass fits by construction (or skips at the cap).
        if cache.glyphs.generation != glyphs_gen || cache.emoji.generation != emoji_gen {
            cache.reset_for(glyphs_gen, emoji_gen);
            continue;
        }
        return FrameData {
            vertices,
            batches,
            glyph_atlas: cache.glyphs.info(),
            emoji_atlas: cache.emoji.info(),
            glyph_upload: cache.glyphs.take_upload(),
            emoji_upload: cache.emoji.take_upload(),
            image_uploads,
        };
    }
}

struct Batcher<'a> {
    fonts: &'a Fonts,
    store: &'a ImageStore,
    cache: &'a mut GlyphCache,
    vertices: Vec<Vertex>,
    batches: Vec<Batch>,
    image_uploads: Vec<TextureKey>,
    /// What is in `image_uploads`, so an image drawn many times is listed once
    /// without searching the list for every quad of a large grid.
    upload_queued: HashSet<TextureKey>,
    known_images: &'a dyn Fn(&TextureKey) -> bool,
}

impl Batcher<'_> {
    fn command(&mut self, cmd: &DrawCmd) {
        match cmd {
            DrawCmd::Fill { rect, color } => {
                self.quad(*rect, MODE_SOLID, *color, [0.0; 2], [0.0; 4]);
            }
            DrawCmd::RoundRect {
                rect,
                color,
                radius,
                corners,
            } => self.round_rect(*rect, *color, *radius, *corners),
            DrawCmd::Text {
                x,
                baseline,
                face,
                color,
                text,
                ..
            } => self.text(*face, *x, *baseline, text, *color),
            DrawCmd::Image {
                rect,
                key,
                generation,
            } => self.image(*rect, key, *generation),
        }
    }

    /// Emit one axis-aligned quad (two triangles) into `image`'s texture, with
    /// uv running from `[u0, v0]` at the top-left to `[u1, v1]` at the
    /// bottom-right (`uv` is `[u0, v0, u1, v1]`). The atlases and fills pass
    /// texel coordinates (uv advancing 1:1 with pixels); an image passes
    /// normalised `0..1` corners so the sampler scales its native texture across
    /// the quad.
    fn quad_uv(
        &mut self,
        rect: Rect,
        mode: u32,
        color: u32,
        uv: [f32; 4],
        extra: [f32; 4],
        image: Option<TextureKey>,
    ) {
        if rect.w <= 0 || rect.h <= 0 {
            return;
        }
        let (x0, y0) = (rect.x as f32, rect.y as f32);
        let (x1, y1) = ((rect.x + rect.w) as f32, (rect.y + rect.h) as f32);
        let c = color_f32(color);
        let v = |x: f32, y: f32, u: f32, vv: f32| Vertex {
            pos: [x, y],
            uv: [u, vv],
            color: c,
            extra,
            mode,
        };
        let [u0, v0, u1, v1] = uv;
        self.vertices.extend_from_slice(&[
            v(x0, y0, u0, v0),
            v(x1, y0, u1, v0),
            v(x0, y1, u0, v1),
            v(x1, y0, u1, v0),
            v(x1, y1, u1, v1),
            v(x0, y1, u0, v1),
        ]);
        self.extend_batch(image, 6);
    }

    /// Emit one axis-aligned quad with texel UVs: `uv0` is the coordinate at the
    /// quad's top-left and uv advances 1:1 with pixels. The atlas-bound path (no
    /// per-image texture), for fills, glyphs, emoji, and rounded corners.
    fn quad(&mut self, rect: Rect, mode: u32, color: u32, uv0: [f32; 2], extra: [f32; 4]) {
        let uv = [
            uv0[0],
            uv0[1],
            uv0[0] + rect.w as f32,
            uv0[1] + rect.h as f32,
        ];
        self.quad_uv(rect, mode, color, uv, extra, None);
    }

    /// Append `count` vertices to the current batch, or open a new one when
    /// the bound image texture would change.
    fn extend_batch(&mut self, image: Option<TextureKey>, count: u32) {
        let start = self.vertices.len() as u32 - count;
        if let Some(last) = self.batches.last_mut() {
            if last.image == image {
                last.count += count;
                return;
            }
        }
        self.batches.push(Batch {
            start,
            count,
            image,
        });
    }

    fn round_rect(&mut self, rect: Rect, color: u32, radius: i32, corners: RoundedCorners) {
        // Clamp the radius to half the shorter side so opposite corners never
        // overlap; the shader's coverage function assumes this bound holds.
        let r = radius.clamp(0, (rect.w / 2).min(rect.h / 2).max(0));
        let mask = match corners {
            RoundedCorners::Top => 1u32,
            RoundedCorners::Both => 3,
        };
        if r == 0 {
            self.quad(rect, MODE_SOLID, color, [0.0; 2], [0.0; 4]);
            return;
        }
        // uv carries the position within the rect (pixels); extra carries the
        // geometry the shader's coverage function needs.
        let extra = [rect.w as f32, rect.h as f32, r as f32, mask as f32];
        self.quad(rect, MODE_ROUND, color, [0.0; 2], extra);
    }

    /// Emit a run's glyph quads: the ASCII fast path, then grapheme clusters
    /// routed to the emoji glyph or per-character drawing. The routing mirrors
    /// [`Fonts::text_advance`], so the quads land where layout measured.
    fn text(&mut self, key: FaceKey, x: i32, baseline: i32, text: &str, color: u32) {
        let mut pen = x as f32;
        if text.is_ascii() {
            for ch in text.chars() {
                pen = self.scalar(key, ch, pen, baseline, color);
            }
            return;
        }
        for (_, cluster) in grapheme::graphemes(text) {
            match self.packed_cluster(key, cluster) {
                Some(packed) => {
                    self.emit_glyph(&packed, pen, baseline, MODE_EMOJI, 0);
                    pen += packed.advance;
                }
                None => {
                    for ch in cluster.chars() {
                        pen = self.scalar(key, ch, pen, baseline, color);
                    }
                }
            }
        }
    }

    /// One character's glyph quad (or nothing for empty rasters like spaces),
    /// returning the advanced pen.
    fn scalar(&mut self, key: FaceKey, ch: char, pen: f32, baseline: i32, color: u32) -> f32 {
        let packed = self.packed_scalar(key, ch);
        self.emit_glyph(&packed, pen, baseline, MODE_GLYPH, color);
        pen + packed.advance
    }

    /// The cached placement for a scalar glyph. On the first sight of this
    /// (face, character) the glyph is rasterized once and its coverage written
    /// straight into the atlas; later frames read the cached placement.
    ///
    /// The glyph comes from whichever face can draw it (see [`Fonts::glyph_face`])
    /// and the pen advances by that face's advance, which is what
    /// [`Fonts::advance`] measures.
    fn packed_scalar(&mut self, key: FaceKey, ch: char) -> PackedGlyph {
        if let Some(&packed) = self.cache.scalar_slots.get(&(key, ch)) {
            self.cache.stats.hits += 1;
            return packed;
        }
        self.cache.stats.misses += 1;
        let g = self.fonts.glyph_face(key, ch).rasterize(ch);
        let packed = PackedGlyph {
            slot: self
                .cache
                .glyphs
                .pack(g.width as u32, g.rows as u32, &g.coverage),
            left: g.left,
            top: g.top,
            advance: g.advance,
        };
        self.cache.remember_scalar((key, ch), packed);
        packed
    }

    /// The cached placement for an emoji cluster, or `None` when the cluster is
    /// not colour emoji and the caller should draw its characters instead. The
    /// raster is packed into the colour atlas on first sight and the decision
    /// cached either way, so a steady-state frame neither re-shapes nor touches
    /// the `Face`.
    fn packed_cluster(&mut self, key: FaceKey, cluster: &str) -> Option<PackedGlyph> {
        if let Some(&packed) = self
            .cache
            .cluster_slots
            .get(&key)
            .and_then(|by| by.get(cluster))
        {
            if packed.is_some() {
                self.cache.stats.hits += 1;
            }
            return packed;
        }
        let glyph = self.fonts.face_for(key).cluster_glyph(cluster);
        let packed = glyph.map(|g| {
            self.cache.stats.misses += 1;
            PackedGlyph {
                slot: pack_argb(
                    &mut self.cache.emoji,
                    g.width as u32,
                    g.rows as u32,
                    &g.argb,
                ),
                left: g.left,
                top: g.top,
                advance: g.advance,
            }
        });
        self.cache.remember_cluster(key, cluster, packed);
        packed
    }

    /// Emit a cached glyph's textured quad. An inkless or unplaced glyph (slot
    /// `None`) draws nothing; the caller advances the pen regardless.
    fn emit_glyph(&mut self, packed: &PackedGlyph, pen: f32, baseline: i32, mode: u32, color: u32) {
        let Some(slot) = packed.slot else {
            return;
        };
        let rect = Rect {
            x: pen.round() as i32 + packed.left,
            y: baseline - packed.top,
            w: slot.w as i32,
            h: slot.h as i32,
        };
        self.quad(rect, mode, color, [slot.x as f32, slot.y as f32], [0.0; 4]);
    }

    /// An image quad over its own texture, naming the texture for upload when
    /// the renderer does not have it yet. An image the store does not hold (not
    /// decoded yet, or evicted) draws nothing.
    fn image(&mut self, rect: Rect, image: &ImageKey, generation: u64) {
        if rect.w <= 0 || rect.h <= 0 {
            return;
        }
        // The image is uploaded at its native size and the GPU sampler scales it
        // to the quad, so a resize reuses the texture: no CPU resample or
        // re-upload happens per drag. The list was painted against this store, so
        // a generation that no longer matches is an image replaced since.
        if self.store.generation(image) != Some(generation) {
            return;
        }
        let key = TextureKey {
            image: image.clone(),
            generation,
        };
        if !(self.known_images)(&key) && self.upload_queued.insert(key.clone()) {
            self.image_uploads.push(key.clone());
        }
        // Normalised UVs so texture() samples the whole native texture across the
        // display rect (quad.frag mode 3, which ignores the vertex colour).
        self.quad_uv(
            rect,
            MODE_IMAGE,
            0,
            [0.0, 0.0, 1.0, 1.0],
            [0.0; 4],
            Some(key),
        );
    }
}

/// Pack an emoji cluster's straight-`0xAARRGGBB` raster into the color atlas.
/// `None` for a zero-area or malformed glyph or a full atlas; the placement is
/// recorded as `None` so the pen advances without drawing.
fn pack_argb(atlas: &mut Atlas, w: u32, h: u32, argb: &[u32]) -> Option<Slot> {
    if w == 0 || h == 0 || argb.len() < (w * h) as usize {
        return None;
    }
    // ARGB words are B,G,R,A bytes in memory: B8G8R8A8 verbatim.
    let bytes: Vec<u8> = argb.iter().flat_map(|px| px.to_ne_bytes()).collect();
    atlas.pack(w, h, &bytes)
}

/// A 0x00RRGGBB display-list color as straight-alpha RGBA floats.
fn color_f32(color: u32) -> [f32; 4] {
    [
        ((color >> 16) & 0xff) as f32 / 255.0,
        ((color >> 8) & 0xff) as f32 / 255.0,
        (color & 0xff) as f32 / 255.0,
        1.0,
    ]
}

#[cfg(test)]
mod tests {
    use crate::platform::freetype::{FaceKey, FontConfig, FontSelection, FontStyle, Fonts};
    use crate::platform::geom::Rect;
    use crate::render::display::{DrawCmd, RoundedCorners};
    use crate::render::gpu::*;
    use crate::render::image::ImageStore;

    const REGULAR: FaceKey = FaceKey {
        size: 16,
        style: FontStyle::Regular,
    };

    #[test]
    fn atlas_packs_onto_shelves_and_tracks_dirty() {
        let mut a = Atlas::new(1);
        let s1 = a.reserve(10, 8).expect("fits");
        let s2 = a.reserve(6, 8).expect("fits on the same shelf");
        assert_eq!((s1.x, s1.y), (0, 0));
        assert_eq!((s2.x, s2.y), (10, 0), "same shelf, packed rightward");
        let s3 = a.reserve(10, 20).expect("taller: a new shelf");
        assert_eq!(s3.y, 8, "new shelf below the first");
        a.write(s1, &[7u8; 80]);
        let up = a.take_upload().expect("dirty after write");
        assert_eq!((up.x, up.y, up.w, up.h), (0, 0, 10, 8));
        assert!(up.bytes.iter().all(|&b| b == 7));
        assert!(a.take_upload().is_none(), "dirty cleared");
    }

    #[test]
    fn atlas_grows_by_wiping_and_bumping_generation() {
        let mut a = Atlas::new(1);
        let g0 = a.generation;
        // A raster wider than the atlas forces growth.
        let s = a.reserve(ATLAS_START + 1, 4).expect("grows to fit");
        assert!(a.generation > g0);
        assert_eq!(a.width, ATLAS_START * 2);
        assert_eq!((s.x, s.y), (0, 0), "the grown atlas starts empty");
        // Beyond the cap is a clean None.
        assert!(a.reserve(ATLAS_MAX + 1, 1).is_none());
    }

    /// A device whose largest texture is smaller than the atlas cap could not
    /// upload an atlas grown past it, so growth stops at the device's limit and
    /// the raster that does not fit is skipped.
    #[test]
    fn an_atlas_never_grows_past_the_devices_largest_texture() {
        let mut cache = GlyphCache::new();
        cache.limit_atlases(2 * ATLAS_START);
        let wide = cache.glyphs.reserve(2 * ATLAS_START, 4);
        assert!(wide.is_some(), "grows up to the limit");
        assert_eq!(cache.glyphs.info().width, 2 * ATLAS_START);
        assert!(cache.glyphs.reserve(2 * ATLAS_START + 1, 4).is_none());
        assert_eq!(cache.glyphs.info().width, 2 * ATLAS_START, "and no further");
        // A limit below the starting size cannot shrink an atlas that exists.
        cache.limit_atlases(16);
        assert!(cache.emoji.reserve(ATLAS_START, 4).is_some());
        assert_eq!(cache.emoji.info().width, ATLAS_START);
    }

    #[test]
    fn fill_becomes_one_solid_quad_batch() {
        let fonts = Fonts::new(&[16]).expect("default font");
        let images = ImageStore::new();
        let mut cache = GlyphCache::new();
        let list = vec![DrawCmd::Fill {
            rect: Rect {
                x: 1,
                y: 2,
                w: 10,
                h: 20,
            },
            color: 0x00ff_0000,
        }];
        let f = build_frame(&fonts, &images, &list, &mut cache, &|_| false);
        assert_eq!(f.vertices.len(), 6);
        assert_eq!(f.batches.len(), 1);
        assert_eq!(
            f.batches[0],
            Batch {
                start: 0,
                count: 6,
                image: None
            }
        );
        assert!(f.vertices.iter().all(|v| v.mode == MODE_SOLID));
        assert_eq!(f.vertices[0].pos, [1.0, 2.0]);
        assert_eq!(f.vertices[4].pos, [11.0, 22.0]);
        assert_eq!(f.vertices[0].color, [1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn text_reuses_atlas_slots_across_frames() {
        let fonts = Fonts::new(&[16]).expect("default font");
        let images = ImageStore::new();
        let mut cache = GlyphCache::new();
        let list = vec![DrawCmd::Text {
            bounds: Rect {
                x: 0,
                y: 0,
                w: 100,
                h: 20,
            },
            x: 0,
            baseline: 16,
            face: REGULAR,
            color: 0x00ff_ffff,
            text: "abcabc".to_string(),
        }];
        let f1 = build_frame(&fonts, &images, &list, &mut cache, &|_| false);
        assert!(f1.glyph_upload.is_some(), "first frame uploads new glyphs");
        assert!(
            !f1.vertices.is_empty(),
            "glyph quads were emitted ({} vertices)",
            f1.vertices.len()
        );
        let f2 = build_frame(&fonts, &images, &list, &mut cache, &|_| false);
        assert!(f2.glyph_upload.is_none(), "steady state uploads nothing");
        assert_eq!(f1.vertices, f2.vertices, "same list, same geometry");
    }

    #[test]
    fn glyph_quads_share_one_batch_with_fills() {
        let fonts = Fonts::new(&[16]).expect("default font");
        let images = ImageStore::new();
        let mut cache = GlyphCache::new();
        let list = vec![
            DrawCmd::Fill {
                rect: Rect {
                    x: 0,
                    y: 0,
                    w: 5,
                    h: 5,
                },
                color: 0,
            },
            DrawCmd::Text {
                bounds: Rect {
                    x: 0,
                    y: 0,
                    w: 100,
                    h: 20,
                },
                x: 0,
                baseline: 16,
                face: REGULAR,
                color: 0x00ff_ffff,
                text: "hi".to_string(),
            },
        ];
        let f = build_frame(&fonts, &images, &list, &mut cache, &|_| false);
        assert_eq!(
            f.batches.len(),
            1,
            "no image, so one batch: {:?}",
            f.batches
        );
        assert_eq!(f.batches[0].count, f.vertices.len() as u32);
    }

    #[test]
    fn round_rect_emits_sdf_quad_with_geometry_in_extra() {
        let fonts = Fonts::new(&[16]).expect("default font");
        let images = ImageStore::new();
        let mut cache = GlyphCache::new();
        let rect = Rect {
            x: 4,
            y: 6,
            w: 20,
            h: 10,
        };
        let list = vec![DrawCmd::RoundRect {
            rect,
            color: 0x0011_2233,
            radius: 4,
            corners: RoundedCorners::Top,
        }];
        let f = build_frame(&fonts, &images, &list, &mut cache, &|_| false);
        assert_eq!(f.vertices.len(), 6);
        let v = &f.vertices[0];
        assert_eq!(v.mode, MODE_ROUND);
        assert_eq!(v.extra, [20.0, 10.0, 4.0, 1.0], "w, h, r, corner mask");
        assert_eq!(v.uv, [0.0, 0.0], "uv is rect-local pixels");
        assert_eq!(f.vertices[4].uv, [20.0, 10.0]);
    }

    #[test]
    fn zero_radius_round_rect_degrades_to_solid() {
        let fonts = Fonts::new(&[16]).expect("default font");
        let images = ImageStore::new();
        let mut cache = GlyphCache::new();
        let list = vec![DrawCmd::RoundRect {
            rect: Rect {
                x: 0,
                y: 0,
                w: 8,
                h: 8,
            },
            color: 0,
            radius: 0,
            corners: RoundedCorners::Both,
        }];
        let f = build_frame(&fonts, &images, &list, &mut cache, &|_| false);
        assert!(f.vertices.iter().all(|v| v.mode == MODE_SOLID));
    }

    #[test]
    fn image_splits_its_own_batch_and_uploads_once() {
        let fonts = Fonts::new(&[16]).expect("default font");
        let bytes = include_bytes!("testdata/tiny.png");
        let decoded = crate::render::image::Image::decode(bytes, Default::default())
            .expect("decode the fixture");
        let mut images = ImageStore::new();
        let tiny = ImageKey::full("tiny.png");
        images.insert(tiny.clone(), decoded);
        let generation = images.generation(&tiny).expect("resident");
        let mut cache = GlyphCache::new();
        let rect = Rect {
            x: 0,
            y: 10,
            w: 8,
            h: 8,
        };
        let fill = DrawCmd::Fill {
            rect: Rect {
                x: 0,
                y: 0,
                w: 4,
                h: 4,
            },
            color: 0,
        };
        let list = vec![
            fill.clone(),
            DrawCmd::Image {
                rect,
                key: tiny.clone(),
                generation,
            },
            fill,
        ];
        let f = build_frame(&fonts, &images, &list, &mut cache, &|_| false);
        assert_eq!(f.batches.len(), 3, "solid / image / solid: {:?}", f.batches);
        // The texture is keyed by the image, not by the quad it is scaled to, so
        // a resize reuses it.
        let key = TextureKey {
            image: tiny.clone(),
            generation,
        };
        assert_eq!(f.batches[1].image.as_ref(), Some(&key));
        // The frame names the texture to upload but carries no pixel copy: the
        // store still owns them, and the upload reads them from it.
        assert_eq!(f.image_uploads, vec![key.clone()]);
        // A renderer that already has the texture gets no upload.
        let f2 = build_frame(&fonts, &images, &list, &mut cache, &|k| *k == key);
        assert!(f2.image_uploads.is_empty());
        assert_eq!(f2.batches.len(), 3, "batches still split around the image");

        // The file is rewritten and decoded again: same key, same size. The
        // texture the renderer holds is of the old pixels, so a new one is asked
        // for rather than the old one being drawn.
        let again = crate::render::image::Image::decode(bytes, Default::default())
            .expect("decode the fixture");
        images.insert(tiny.clone(), again);
        let generation = images.generation(&tiny).expect("resident");
        let list = vec![DrawCmd::Image {
            rect,
            key: tiny.clone(),
            generation,
        }];
        let f3 = build_frame(&fonts, &images, &list, &mut cache, &|k| *k == key);
        assert_eq!(
            f3.image_uploads,
            vec![TextureKey {
                image: tiny,
                generation
            }]
        );
    }

    #[test]
    fn an_image_the_store_does_not_hold_draws_nothing() {
        let fonts = Fonts::new(&[16]).expect("default font");
        let images = ImageStore::new();
        let mut cache = GlyphCache::new();
        let list = vec![DrawCmd::Image {
            rect: Rect {
                x: 0,
                y: 0,
                w: 8,
                h: 8,
            },
            key: ImageKey::full("nope.png"),
            generation: 0,
        }];
        let f = build_frame(&fonts, &images, &list, &mut cache, &|_| false);
        assert!(f.vertices.is_empty());
        assert!(f.batches.is_empty());
        assert!(f.image_uploads.is_empty());
    }

    /// A one-run display list at the regular face.
    fn run(text: &str) -> Vec<DrawCmd> {
        vec![DrawCmd::Text {
            bounds: Rect {
                x: 0,
                y: 0,
                w: 400,
                h: 20,
            },
            x: 0,
            baseline: 16,
            face: REGULAR,
            color: 0x00ff_ffff,
            text: text.to_string(),
        }]
    }

    /// The right edge of the last quad drawn in `mode`.
    fn right_edge(frame: &FrameData, mode: u32) -> f32 {
        frame
            .vertices
            .iter()
            .filter(|v| v.mode == mode)
            .map(|v| v.pos[0])
            .fold(f32::MIN, f32::max)
    }

    #[test]
    fn emoji_in_a_file_name_draws_from_the_colour_atlas() {
        let selection = FontSelection::resolve(&FontConfig::default()).expect("a font");
        if selection.emoji.is_none() {
            eprintln!("no colour emoji font installed; skipping");
            return;
        }
        let fonts = Fonts::with_selection(&selection, &[16]).expect("fonts");
        let images = ImageStore::new();
        let mut cache = GlyphCache::new();
        // A flag is two scalars that only the emoji font's ligature joins.
        let f = build_frame(&fonts, &images, &run("🇳🇴"), &mut cache, &|_| false);
        let emoji = f.vertices.iter().filter(|v| v.mode == MODE_EMOJI).count();
        assert_eq!(emoji, 6, "one colour quad for the whole flag");
        assert!(f.vertices.iter().all(|v| v.mode != MODE_GLYPH), "no tofu");
        assert!(f.emoji_upload.is_some(), "the colour raster was uploaded");
    }

    /// A character the text family lacks is drawn from a system font, and the pen
    /// must move by that font's advance: spacing wide CJK glyphs by the family's
    /// narrow `.notdef` box overlaps them.
    #[test]
    fn a_fallback_glyph_advances_by_the_font_that_draws_it() {
        let fonts = Fonts::new(&[16]).expect("default font");
        // Whichever of these the family lacks and some installed font draws wider or
        // narrower than the family's `.notdef` box.
        let differs =
            |ch: &char| fonts.face_for(REGULAR).advance(*ch) != fonts.advance(REGULAR, *ch);
        let Some(ch) = ['漢', 'א', 'ก', 'あ'].into_iter().find(differs) else {
            eprintln!("no installed fallback font to tell apart; skipping");
            return;
        };
        let images = ImageStore::new();
        let mut cache = GlyphCache::new();
        let (one, two) = (ch.to_string(), format!("{ch}{ch}"));
        let first = build_frame(&fonts, &images, &run(&one), &mut cache, &|_| false);
        let both = build_frame(&fonts, &images, &run(&two), &mut cache, &|_| false);
        let step = right_edge(&both, MODE_GLYPH) - right_edge(&first, MODE_GLYPH);
        assert_eq!(step, fonts.advance(REGULAR, ch).round());
        assert_eq!(
            fonts.text_advance(REGULAR, &two),
            2.0 * fonts.advance(REGULAR, ch),
            "layout measures what the batcher draws"
        );
    }

    #[test]
    fn a_raster_shorter_than_its_size_is_refused_not_read_past() {
        let mut a = Atlas::new(1);
        assert!(a.pack(4, 4, &[0xff; 15]).is_none());
        assert!(a.pack(4, 4, &[0xff; 16]).is_some());
    }

    #[test]
    fn the_slot_maps_are_dropped_at_their_cap_and_the_frame_still_draws() {
        let fonts = Fonts::new(&[16]).expect("default font");
        let images = ImageStore::new();
        let mut cache = GlyphCache::new();
        // More distinct scalars than the map holds, a screenful at a time.
        let scalars: Vec<char> = (0x4E00..0x4E00 + MAX_SLOTS as u32 + 64)
            .filter_map(char::from_u32)
            .collect();
        for line in scalars.chunks(64) {
            let text: String = line.iter().collect();
            build_frame(&fonts, &images, &run(&text), &mut cache, &|_| false);
        }
        assert!(cache.scalar_slots.len() <= MAX_SLOTS);
        assert!(
            cache.glyphs.generation > 0,
            "the atlas went with its records"
        );
        // The frame that crossed the cap is whole: every glyph of its line placed.
        let f = build_frame(&fonts, &images, &run("ab"), &mut cache, &|_| false);
        assert_eq!(f.vertices.len(), 12);
    }
}
