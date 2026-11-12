//! Thumbnail-grid geometry: square cells, or masonry columns.
//!
//! Pure layout math: given the viewport, image count, and scroll offset, it works
//! out the column count and where each cell lands, and maps a window point back to
//! a cell for click selection. The renderer and the input handler share this, so
//! they always agree on where things are.
//!
//! Everything here is in *device* pixels. The cell size, gap, and padding are
//! authored as logical pixels and scaled on the way in ([`Metrics::new`]), so the
//! grid looks the same physical size on a 100% and a 200% display.
//!
//! The viewport height is the caller's: the grid is drawn above the status bar,
//! and passing the full window height instead would hide the last row behind it.

use std::ops::Range;
use std::rc::Rc;

use crate::masonry::{self, Masonry, Tile};
use crate::platform::geom::Scale;

/// Thumbnail cell size, gap between cells, and outer padding, in logical pixels.
pub const THUMB: i32 = 160;
pub const SPACING: i32 = 8;
pub const PAD: i32 = 16;

/// The grid's box model at one display scale.
///
/// A cell is the thumbnail square *plus* the line of text naming it. Sizing the
/// pitch off the thumbnail alone would run each row's labels under the row below:
/// the eight pixels of spacing are a gap between cells, not room for a line of
/// text.
#[derive(Clone, Copy)]
pub struct Metrics {
    /// The side of a thumbnail square.
    pub thumb: i32,
    /// The height of a cell's label line, below its thumbnail.
    pub label: i32,
    /// The full height of a cell: the thumbnail and its label.
    pub cell_h: i32,
    /// Distance between cell origins (cell height + spacing).
    pub pitch: i32,
    /// Padding between the grid and the edge of its viewport.
    pub pad: i32,
}

impl Metrics {
    /// `label_h` is the line height of the font the cell captions are drawn in,
    /// which only the caller knows: the fonts are opened at the display scale.
    pub fn new(scale: Scale, label_h: i32) -> Self {
        let thumb = scale.px(THUMB).max(1);
        let label = label_h.max(0);
        let cell_h = thumb + label;
        Self {
            thumb,
            label,
            cell_h,
            pitch: cell_h + scale.px(SPACING).max(1),
            pad: scale.px(PAD),
        }
    }

    /// The same box model, for masonry: columns at least a square cell wide.
    pub fn masonry(&self) -> masonry::Metrics {
        masonry::Metrics {
            column: self.thumb,
            label: self.label,
            gap: self.pitch - self.cell_h,
            pad: self.pad,
        }
    }
}

/// How the cells are placed: in square cells, or in masonry columns laid out
/// beforehand (see [`crate::masonry`]).
enum Tiling {
    Square { columns: usize },
    Masonry(Rc<Masonry>),
}

pub struct Layout {
    /// Vertical scroll, clamped to the valid range for this content.
    pub scroll: f32,
    pub max_scroll: f32,
    metrics: Metrics,
    tiling: Tiling,
}

impl Layout {
    /// Lay out `count` thumbnails in square cells in a `w` x `h` viewport at the
    /// desired `scroll` (which is clamped to the content's range).
    pub fn new(metrics: Metrics, w: i32, h: i32, count: usize, scroll: f32) -> Layout {
        let pad = metrics.pad as f32;
        let gap = (metrics.pitch - metrics.cell_h) as f32;
        // Columns are decided by the thumbnail's width, which is the cell's width;
        // only the height carries the label.
        let column_pitch = (metrics.thumb as f32) + gap;
        let avail = (w as f32 - 2.0 * pad + gap).max(column_pitch);
        let columns = ((avail / column_pitch).floor() as usize).max(1);
        let rows = count.div_ceil(columns);
        let content = 2.0 * pad + rows as f32 * metrics.pitch as f32 - gap;
        let max_scroll = (content - h as f32).max(0.0);
        Layout {
            scroll: scroll.clamp(0.0, max_scroll),
            max_scroll,
            metrics,
            tiling: Tiling::Square { columns },
        }
    }

    /// Masonry `columns`, already laid out for the viewport's width, in a
    /// viewport `h` tall at the desired `scroll`.
    pub fn masonry(metrics: Metrics, columns: Rc<Masonry>, h: i32, scroll: f32) -> Layout {
        let max_scroll = (columns.content - h as f32).max(0.0);
        Layout {
            scroll: scroll.clamp(0.0, max_scroll),
            max_scroll,
            metrics,
            tiling: Tiling::Masonry(columns),
        }
    }

    /// The full height of the laid-out content, scrolled or not: what a scrollbar
    /// measures its thumb against.
    pub fn content_height(&self, h: i32) -> i32 {
        h + self.max_scroll as i32
    }

    /// The horizontal distance between cell origins: the thumbnail's width plus the
    /// gap. Columns are as wide as a thumbnail; only rows are as tall as a label.
    fn column_pitch(&self) -> f32 {
        (self.metrics.pitch - self.metrics.cell_h + self.metrics.thumb) as f32
    }

    /// Cell `i`'s picture box in viewport coordinates (scroll applied). Its
    /// caption is the label line under it, as wide as it.
    pub fn tile(&self, i: usize) -> Option<Tile> {
        match &self.tiling {
            Tiling::Square { columns } => {
                let col = (i % columns) as f32;
                let row = (i / columns) as f32;
                let pad = self.metrics.pad as f32;
                let side = self.metrics.thumb as f32;
                Some(Tile {
                    x: pad + col * self.column_pitch(),
                    y: pad + row * self.metrics.pitch as f32 - self.scroll,
                    w: side,
                    h: side,
                })
            }
            Tiling::Masonry(columns) => columns.tile(i).map(|t| Tile {
                y: t.y - self.scroll,
                ..t
            }),
        }
    }

    /// The cells, of `count`, whose top edge lies strictly between `top` and
    /// `bottom` in viewport coordinates. Whole rows of the grid, so the answer
    /// is a range and costs the same in a folder of ten files or a million.
    pub fn cells_between(&self, count: usize, top: f32, bottom: f32) -> Range<usize> {
        let columns = match &self.tiling {
            Tiling::Square { columns } => *columns,
            Tiling::Masonry(columns) => {
                let r = columns.between(top + self.scroll, bottom + self.scroll);
                return r.start.min(count)..r.end.min(count);
            }
        };
        let pitch = self.metrics.pitch as f32;
        let offset = self.scroll - self.metrics.pad as f32;
        // Row `r` starts at `r * pitch - offset`. The casts saturate, so a bound
        // above the content gives row 0 and one far below it gives a row past
        // the end, which the clamp to `count` then trims.
        let first = (((top + offset) / pitch).floor() + 1.0).max(0.0) as usize;
        let end = ((bottom + offset) / pitch).ceil().max(0.0) as usize;
        let start = first.saturating_mul(columns).min(count);
        let end = end.saturating_mul(columns).min(count);
        start..end.max(start)
    }

    /// The cells, of `count`, any part of which (caption included) lies
    /// between `top` and `bottom` in viewport coordinates: what is drawn, and
    /// what is worth decoding, however tall a masonry tile runs.
    pub fn cells_overlapping(&self, count: usize, top: f32, bottom: f32) -> Range<usize> {
        match &self.tiling {
            Tiling::Square { .. } => {
                self.cells_between(count, top - self.metrics.cell_h as f32, bottom)
            }
            Tiling::Masonry(columns) => {
                let r = columns.overlapping(top + self.scroll, bottom + self.scroll);
                r.start.min(count)..r.end.min(count)
            }
        }
    }

    /// The cell at viewport point `(px, py)`, if the point lands on a cell rather
    /// than in the padding, in a gap, or past the last item.
    ///
    /// The cell includes its label: clicking a file's name selects the file, which
    /// is what clicking a name obviously ought to do.
    pub fn hit(&self, count: usize, px: f32, py: f32) -> Option<usize> {
        let columns = match &self.tiling {
            Tiling::Square { columns } => *columns,
            Tiling::Masonry(columns) => {
                return columns.hit(px, py + self.scroll).filter(|&i| i < count);
            }
        };
        let pad = self.metrics.pad as f32;
        let column_pitch = self.column_pitch();
        let pitch = self.metrics.pitch as f32;
        if px < pad || py < pad - self.scroll {
            return None;
        }
        let col_f = (px - pad) / column_pitch;
        let row_f = (py + self.scroll - pad) / pitch;
        let col = col_f.floor() as usize;
        let row = row_f.floor() as usize;
        if col >= columns {
            return None;
        }
        // Reject the gap to the right of / below a cell. The cell spans [0, thumb)
        // across and [0, cell_h) down, so a point exactly at either edge is the
        // gap's first pixel and belongs to no cell.
        let in_col = col_f.fract() * column_pitch;
        let in_row = row_f.fract() * pitch;
        if in_col >= self.metrics.thumb as f32 || in_row >= self.metrics.cell_h as f32 {
            return None;
        }
        let i = row * columns + col;
        (i < count).then_some(i)
    }

    /// The cell, of `count`, `rows` rows above (negative) or below cell `from`.
    /// In squares, the same column, and past either end the first or the last
    /// cell; in masonry, down its own column, stopping at the column's ends.
    pub fn vertical(&self, count: usize, from: usize, rows: i32) -> usize {
        let last = count.saturating_sub(1);
        match &self.tiling {
            Tiling::Square { columns } => {
                let to = from as i64 + rows as i64 * *columns as i64;
                to.clamp(0, last as i64) as usize
            }
            Tiling::Masonry(columns) => columns.vertical(from, rows).min(last),
        }
    }

    /// The cell, of `count`, one to the left (`dir` -1) or right (1) of cell
    /// `from`: the next along in squares; in masonry, the one level with it in
    /// the next column, or the next along from an edge column.
    pub fn horizontal(&self, count: usize, from: usize, dir: i32) -> usize {
        let last = count.saturating_sub(1);
        match &self.tiling {
            Tiling::Square { .. } => (from as i64 + dir as i64).clamp(0, last as i64) as usize,
            Tiling::Masonry(columns) => columns.horizontal(from, dir).min(last),
        }
    }

    /// How many rows a viewport `h` tall shows: what a page moves by.
    pub fn rows_per_page(&self, h: i32) -> i32 {
        let pitch = match &self.tiling {
            Tiling::Square { .. } => self.metrics.pitch,
            Tiling::Masonry(columns) => columns.pitch() as i32,
        };
        (h / pitch.max(1)).max(1)
    }

    /// The scroll that brings cell `i` and its caption fully into a viewport
    /// `h` tall, moving as little as it can.
    pub fn reveal(&self, i: usize, h: i32) -> f32 {
        let Some(tile) = self.tile(i) else {
            return self.scroll;
        };
        crate::platform::scroll::reveal(
            self.scroll as i32,
            (tile.y + self.scroll) as i32,
            tile.h as i32 + self.metrics.label,
            h,
            self.content_height(h),
            self.metrics.pad,
        ) as f32
    }

    /// The cell, of `count`, in the middle of a viewport `h` tall: where the
    /// eye is while the wheel scrolls.
    pub fn centre(&self, count: usize, h: i32) -> usize {
        let last = count.saturating_sub(1);
        let middle = self.scroll + h as f32 / 2.0;
        match &self.tiling {
            Tiling::Square { columns } => {
                let m = self.metrics;
                let row = ((middle - m.pad as f32) / m.pitch as f32).max(0.0) as usize;
                (row * columns + columns / 2).min(last)
            }
            Tiling::Masonry(columns) => columns.at_height(middle).min(last),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::grid::{Layout, Metrics, Tiling, PAD, SPACING, THUMB};
    use crate::platform::geom::Scale;

    /// A 20px caption line, so the arithmetic below stays in round numbers.
    const LABEL: i32 = 20;

    fn metrics() -> Metrics {
        Metrics::new(Scale::ONE, LABEL)
    }

    fn columns(l: &Layout) -> usize {
        match l.tiling {
            Tiling::Square { columns } => columns,
            Tiling::Masonry(_) => 0,
        }
    }

    #[test]
    fn columns_fit_the_width() {
        // 16 pad + 160 + 8 + 160 + 8 + 160 + 16 = 528 fits exactly 3 columns. The
        // label makes cells taller, never wider, so the column count is unmoved.
        let l = Layout::new(metrics(), 528, 600, 10, 0.0);
        assert_eq!(columns(&l), 3);
    }

    #[test]
    fn a_row_is_tall_enough_for_its_own_labels() {
        // The bug this pins: a pitch sized off the thumbnail alone draws every
        // row's captions under the row below it.
        let m = metrics();
        assert_eq!(m.cell_h, THUMB + LABEL);
        assert_eq!(m.pitch, THUMB + LABEL + SPACING);

        // Cell 0's label occupies [thumb, thumb + label) of its cell, and the next
        // row starts a whole spacing after that.
        let l = Layout::new(m, 528, 600, 10, 0.0);
        let first = l.tile(0).unwrap().y;
        let second = l.tile(3).unwrap().y; // the row below, at 3 columns
        assert!(
            first + m.cell_h as f32 <= second,
            "row 2 starts at {second}, over row 1's labels ending at {}",
            first + m.cell_h as f32
        );
    }

    #[test]
    fn scroll_clamps_to_content() {
        // 10 items, 3 cols -> 4 rows. content = 16*2 + 4*188 - 8 = 776.
        let expected = 2 * PAD + 4 * (THUMB + LABEL + SPACING) - SPACING;
        let l = Layout::new(metrics(), 528, 600, 10, 100_000.0);
        assert_eq!(l.max_scroll, (expected - 600) as f32);
        assert_eq!(l.scroll, l.max_scroll);
        assert_eq!(l.content_height(600), expected);
    }

    #[test]
    fn hit_finds_a_cell_and_rejects_the_gaps_around_it() {
        let l = Layout::new(metrics(), 528, 600, 10, 0.0);
        assert_eq!(l.hit(10, 20.0, 20.0), Some(0)); // inside cell 0
        assert_eq!(l.hit(10, 200.0, 20.0), Some(1)); // inside cell 1
        assert_eq!(l.hit(10, 5.0, 5.0), None); // in the outer padding

        // Across, a cell is as wide as its thumbnail; its last pixel hits and the
        // gap after it belongs to no cell.
        assert_eq!(l.hit(10, (PAD + THUMB - 1) as f32, 20.0), Some(0));
        assert_eq!(l.hit(10, (PAD + THUMB) as f32, 20.0), None);

        // Down, a cell runs through its label: clicking a file's name selects the
        // file, which is what clicking a name has to do.
        assert_eq!(l.hit(10, 20.0, (PAD + THUMB + 2) as f32), Some(0));
        assert_eq!(l.hit(10, 20.0, (PAD + THUMB + LABEL - 1) as f32), Some(0));
        assert_eq!(l.hit(10, 20.0, (PAD + THUMB + LABEL) as f32), None);
    }

    #[test]
    fn the_grid_grows_with_the_display_scale() {
        // At 200% every cell, gap, and margin doubles in device pixels, so the same
        // window fits half as many columns as it does at 100%. The label height is
        // the caller's, already in device pixels, so it is passed doubled too.
        let m = Metrics::new(Scale::from_120(240), LABEL * 2);
        assert_eq!(m.thumb, THUMB * 2);
        assert_eq!(m.pitch, (THUMB + LABEL + SPACING) * 2);
        assert_eq!(m.pad, PAD * 2);
        let l = Layout::new(m, 528 * 2, 1200, 10, 0.0);
        assert_eq!(columns(&l), 3, "a 2x window at 2x scale holds the same 3");
        let l = Layout::new(m, 528, 600, 10, 0.0);
        assert_eq!(columns(&l), 1, "the same pixels at 2x hold fewer");
    }

    #[test]
    fn cells_between_agrees_with_asking_each_cell() {
        // The range is arithmetic on rows; the definition is each cell's own
        // origin. They must agree wherever the window sits, including above
        // the content, below it, and on an exact row boundary.
        let m = metrics();
        let pitch = m.pitch as f32;
        for count in [0usize, 1, 7, 9, 40] {
            for scroll in [0.0, 37.0, pitch, 3.0 * pitch + 0.5, 1e6] {
                let l = Layout::new(m, 528, 300, count, scroll);
                for (top, bottom) in [
                    (-600.0, 600.0),
                    (0.0, 300.0),
                    (-pitch, pitch),
                    (PAD as f32 - l.scroll, PAD as f32 - l.scroll + pitch),
                    (-1e7, -1e6),
                    (1e6, 1e7),
                    (50.0, 10.0),
                ] {
                    let asked: Vec<usize> = (0..count)
                        .filter(|&i| {
                            let y = l.tile(i).unwrap().y;
                            y > top && y < bottom
                        })
                        .collect();
                    let range: Vec<usize> = l.cells_between(count, top, bottom).collect();
                    assert_eq!(
                        range, asked,
                        "count {count} scroll {scroll} {top}..{bottom}"
                    );
                }
            }
        }
    }
}
