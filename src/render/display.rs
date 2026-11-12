//! The frame as data: a flat list of drawing primitives the painter emits and the
//! backend consumes. Two things hang off it:
//!
//! - **Damage tracking**: because a frame is a value, two frames can be compared.
//!   [`damage`] diffs the previous frame against the new one and returns just the
//!   screen rectangles that changed, so a moved selection reports two rows rather
//!   than the whole surface.
//! - **A backend-agnostic frame**: the list is turned into pixels by the GPU
//!   batcher ([`crate::render::gpu::build_frame`]), which consumes exactly these
//!   commands. Nothing here is Vulkan-specific.
//!
//! Commands are in *screen* space (already shifted by the scroll), so scrolling,
//! which moves every command, diffs as "everything changed" and falls back to a
//! full repaint.
//!
//! ```text
//!   painter ──▶ DisplayList ──┬─ gpu::build_frame ─▶ vertices
//!                             └─ damage(old,new) ─▶ [Rect]
//! ```

use std::collections::HashMap;

use crate::platform::freetype::FaceKey;
use crate::platform::geom::Rect;
use crate::render::image::ImageKey;

/// A frame, as the ordered primitives that draw it. Stacking is the list order:
/// background first, then panels, selection, glyphs, and the caret last.
pub type DisplayList = Vec<DrawCmd>;

/// Beyond this many distinct changed regions, repainting the whole surface is
/// cheaper than tracking and clipping each one.
const MAX_DAMAGE_RECTS: usize = 16;

/// Beyond this many changed regions, the frame repaints whole without merging them,
/// since merging takes time quadratic in the count. A scrolled thumbnail grid moves
/// every cell's image, fill and label, which is thousands of regions that would
/// only merge into "everything" anyway.
const MAX_MERGE_RECTS: usize = 256;

/// Which corners a [`DrawCmd::RoundRect`] rounds: all four, or only the top pair for
/// a panel that sits flush on something below it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RoundedCorners {
    Top,
    Both,
}

/// One drawing primitive in screen (post-scroll) pixels. The set is deliberately
/// small and low level; the painter decomposes higher-level chrome (an image's
/// selection border, a button) into these. Every variant is `Eq + Hash` so
/// [`damage`] can diff two frames by value, which is why text runs carry an owned
/// [`String`] and a borrow-free [`FaceKey`] rather than a `&str`/`&Face` borrowed
/// from the state and the fonts.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum DrawCmd {
    /// A solid, opaque rectangle.
    Fill { rect: Rect, color: u32 },
    /// A solid rectangle with anti-aliased rounded corners.
    RoundRect {
        rect: Rect,
        color: u32,
        radius: i32,
        corners: RoundedCorners,
    },
    /// A run of text drawn from `x` along `baseline`. `bounds` is the run's screen
    /// rectangle padded for glyph ink that overhangs the advance box, so the
    /// damage diff and the clip test never clip a glyph's edge.
    Text {
        bounds: Rect,
        x: i32,
        baseline: i32,
        face: FaceKey,
        color: u32,
        text: String,
    },
    /// A decoded image scaled into `rect`. `key` names it in the image store; one
    /// that is not there draws nothing. `generation` is the store's for the image
    /// the painter saw, so new pixels under an old key are a changed frame.
    Image {
        rect: Rect,
        key: ImageKey,
        generation: u64,
    },
}

impl DrawCmd {
    /// The screen rectangle this command can touch: a region intersecting it must
    /// be repainted, and the command itself is skipped when it misses the clip.
    fn bounds(&self) -> Rect {
        match self {
            DrawCmd::Fill { rect, .. }
            | DrawCmd::RoundRect { rect, .. }
            | DrawCmd::Image { rect, .. } => *rect,
            DrawCmd::Text { bounds, .. } => *bounds,
        }
    }
}

/// The screen regions that differ between the previous frame `old` and the new
/// frame `new`, clamped to the `width` x `height` surface and free of overlaps.
///
/// A multiset difference does the work: a command appearing an unequal number of
/// times in the two frames contributes its bounds, so an identical command (same
/// geometry, face, colour, text) cancels and costs nothing, while a moved or
/// restyled run shows up as a removal plus an addition and damages both spots.
/// A frame that changed most of the surface (a scroll, a resize, a large edit)
/// returns a single full-surface rectangle, since repainting whole then beats
/// clipping many regions. An empty result means the two frames are identical.
pub fn damage(old: &[DrawCmd], new: &[DrawCmd], width: i32, height: i32) -> Vec<Rect> {
    // Dropping an equal pair (one command from each frame) never changes the
    // multiset difference, so strip the common prefix and suffix first. Lists are
    // built in document order, so a blink or a small edit differs only in a
    // handful of commands and the hash work below shrinks to just those;
    // equality bails on the first differing field, cheaper than hashing.
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let (old, new) = (&old[prefix..], &new[prefix..]);
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (old, new) = (&old[..old.len() - suffix], &new[..new.len() - suffix]);

    let mut counts: HashMap<&DrawCmd, i32> = HashMap::new();
    for c in old {
        *counts.entry(c).or_insert(0) += 1;
    }
    for c in new {
        *counts.entry(c).or_insert(0) -= 1;
    }
    let surface = Rect {
        x: 0,
        y: 0,
        w: width.max(0),
        h: height.max(0),
    };
    let mut rects: Vec<Rect> = counts
        .into_iter()
        .filter(|&(_, n)| n != 0)
        .filter_map(|(c, _)| intersection(c.bounds(), surface))
        .collect();
    let surface_area = surface.w as i64 * surface.h as i64;
    let whole = rects.len() > MAX_MERGE_RECTS || {
        coalesce(&mut rects);
        let area: i64 = rects.iter().map(|r| r.w as i64 * r.h as i64).sum();
        rects.len() > MAX_DAMAGE_RECTS || area * 2 > surface_area
    };
    if whole {
        return if surface_area > 0 {
            vec![surface]
        } else {
            Vec::new()
        };
    }
    rects
}

/// Merge any two overlapping rectangles into their bounding box, repeated until
/// none overlap, so a region is never cleared and repainted twice. It sees at most
/// [`MAX_MERGE_RECTS`] rectangles, which keeps the quadratic scan cheap.
fn coalesce(rects: &mut Vec<Rect>) {
    let mut merged = true;
    while merged {
        merged = false;
        'scan: for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                if intersects(rects[i], rects[j]) {
                    rects[i] = union(rects[i], rects[j]);
                    rects.remove(j);
                    merged = true;
                    break 'scan;
                }
            }
        }
    }
}

/// Whether two rectangles share any interior pixel (touching edges do not count):
/// exactly when their [`intersection`] is non-empty.
fn intersects(a: Rect, b: Rect) -> bool {
    intersection(a, b).is_some()
}

/// The smallest rectangle covering both `a` and `b`.
fn union(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.w).max(b.x + b.w);
    let y1 = (a.y + a.h).max(b.y + b.h);
    Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    }
}

/// The overlap of `a` and `b`, or `None` when they are disjoint.
fn intersection(a: Rect, b: Rect) -> Option<Rect> {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.w).min(b.x + b.w);
    let y1 = (a.y + a.h).min(b.y + b.h);
    (x0 < x1 && y0 < y1).then_some(Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

#[cfg(test)]
mod tests {
    use crate::render::display::*;

    fn fill(x: i32, y: i32, w: i32, h: i32, color: u32) -> DrawCmd {
        DrawCmd::Fill {
            rect: Rect { x, y, w, h },
            color,
        }
    }

    const W: i32 = 800;
    const H: i32 = 600;

    #[test]
    fn identical_frames_have_no_damage() {
        let frame = vec![fill(0, 0, W, H, 0x010101), fill(10, 10, 5, 5, 0xffffff)];
        assert!(damage(&frame, &frame, W, H).is_empty());
    }

    #[test]
    fn a_caret_toggle_damages_only_the_caret() {
        // The blink case: the frames differ by one small Fill (the caret).
        let bg = fill(0, 0, W, H, 0x010101);
        let caret = fill(100, 50, 2, 18, 0xffffff);
        let off = vec![bg.clone()];
        let on = vec![bg, caret];
        assert_eq!(
            damage(&off, &on, W, H),
            vec![Rect {
                x: 100,
                y: 50,
                w: 2,
                h: 18
            }],
            "only the caret rectangle is damaged"
        );
    }

    #[test]
    fn a_moved_command_damages_both_positions() {
        // A run that shifts (an edit before it) appears as a remove plus an add;
        // the two non-overlapping spots both need repainting.
        let bg = fill(0, 0, W, H, 0x010101);
        let before = vec![bg.clone(), fill(10, 10, 4, 4, 0xaa)];
        let after = vec![bg, fill(500, 400, 4, 4, 0xaa)];
        let mut d = damage(&before, &after, W, H);
        d.sort_by_key(|r| r.x);
        assert_eq!(
            d,
            vec![
                Rect {
                    x: 10,
                    y: 10,
                    w: 4,
                    h: 4
                },
                Rect {
                    x: 500,
                    y: 400,
                    w: 4,
                    h: 4
                },
            ]
        );
    }

    #[test]
    fn overlapping_changes_coalesce_into_one_region() {
        let bg = fill(0, 0, W, H, 0x010101);
        let old = vec![bg.clone()];
        let new = vec![bg, fill(10, 10, 20, 20, 0x01), fill(20, 20, 20, 20, 0x02)];
        // The two added fills overlap, so the damage is their bounding box.
        assert_eq!(
            damage(&old, &new, W, H),
            vec![Rect {
                x: 10,
                y: 10,
                w: 30,
                h: 30
            }]
        );
    }

    #[test]
    fn a_large_change_falls_back_to_a_full_repaint() {
        // Two big disjoint fills cover more than half the surface, so the diff
        // collapses to one whole-surface rectangle rather than tracking regions.
        let old: Vec<DrawCmd> = Vec::new();
        let new = vec![fill(0, 0, W, H / 2 + 10, 0x01), fill(0, H / 2, W, H, 0x02)];
        assert_eq!(
            damage(&old, &new, W, H),
            vec![Rect {
                x: 0,
                y: 0,
                w: W,
                h: H
            }]
        );
    }

    #[test]
    fn new_pixels_under_the_same_key_damage_the_image() {
        // A file rewritten in place: same key, same rectangle, different picture.
        let rect = Rect {
            x: 10,
            y: 10,
            w: 64,
            h: 64,
        };
        let image = |generation| DrawCmd::Image {
            rect,
            key: ImageKey::full("photo.png"),
            generation,
        };
        assert!(damage(&[image(1)], &[image(1)], W, H).is_empty());
        assert_eq!(damage(&[image(1)], &[image(2)], W, H), vec![rect]);
    }

    #[test]
    fn damage_is_clamped_to_the_surface() {
        // A command partly off-screen damages only its on-surface part; one fully
        // off-screen contributes nothing.
        let on = fill(-5, -5, 20, 20, 0x01);
        let off = fill(W + 100, 0, 10, 10, 0x02);
        let new = vec![on, off];
        assert_eq!(
            damage(&[], &new, W, H),
            vec![Rect {
                x: 0,
                y: 0,
                w: 15,
                h: 15
            }]
        );
    }
}
