//! Masonry: thumbnails in columns of one width, each picture at its own height,
//! uncropped.
//!
//! ```text
//!   ┌──────┐┌──────┐┌──────┐   each picture goes under the column that is
//!   │  0   ││  1   ││      │   shortest so far, so the columns stay level
//!   └──────┘│      ││  2   │   and nothing is left gaping
//!   ┌──────┐└──────┘│      │
//!   │  3   │┌──────┐└──────┘
//!   │      ││  4   │┌──────┐
//!   └──────┘└──────┘│  5   │
//! ```
//!
//! Every picture starts no higher than the one before it (the shortest
//! column only ever gets taller), so the tops still come in the listing's
//! order down the page, and the cells on screen are a range of it, as in the
//! square grid.
//!
//! Pure: the shapes come in as width-to-height ratios, one per entry, and
//! everything here is in device pixels, in *content* coordinates (scroll not
//! applied). Building is linear in the entries; the lookups are binary
//! searches.

use std::ops::Range;

/// The narrowest and widest shape a tile takes. A panorama is shown inside a
/// tile of the widest shape rather than as a sliver a few pixels tall, and a
/// tall strip inside one of the narrowest rather than a whole column's worth.
const MIN_RATIO: f32 = 0.25;
const MAX_RATIO: f32 = 4.0;

/// How the grid lays its thumbnails out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// Masonry when the folder's pictures differ in shape, squares when they
    /// do not.
    Auto,
    /// Every thumbnail in the same square cell.
    Square,
    /// Every picture at its own height, in columns of one width.
    Masonry,
}

impl Shape {
    pub const ALL: [Shape; 3] = [Shape::Auto, Shape::Square, Shape::Masonry];

    /// The next one along, for the key that steps through them.
    pub fn next(self) -> Shape {
        match self {
            Shape::Auto => Shape::Square,
            Shape::Square => Shape::Masonry,
            Shape::Masonry => Shape::Auto,
        }
    }

    /// Its name in the settings file and in the status bar.
    pub fn key(self) -> &'static str {
        match self {
            Shape::Auto => "auto",
            Shape::Square => "square",
            Shape::Masonry => "masonry",
        }
    }

    pub fn from_key(key: &str) -> Option<Shape> {
        Shape::ALL.into_iter().find(|s| s.key() == key)
    }
}

/// How far the pictures' shapes must spread before masonry is worth it: the
/// standard deviation of their log ratios. Landscape photographs all of one
/// camera spread by nothing; a mix of landscape and portrait 3:2 photographs
/// by about 0.4.
const SPREAD: f32 = 0.15;

/// Whether pictures of these shapes (width over height) look better in
/// masonry than in squares: whether they differ in shape enough that a square
/// cell would leave a lot of them small in a lot of empty space.
///
/// Ratios are compared on a log scale, where a portrait and the landscape it
/// is turned from are the same distance either side of square.
pub fn prefers_masonry(ratios: &[f32]) -> bool {
    let logs: Vec<f32> = ratios
        .iter()
        .filter(|r| r.is_finite() && **r > 0.0)
        .map(|r| r.ln())
        .collect();
    if logs.len() < 2 {
        return false;
    }
    let n = logs.len() as f32;
    let mean = logs.iter().sum::<f32>() / n;
    let variance = logs.iter().map(|l| (l - mean).powi(2)).sum::<f32>() / n;
    variance.sqrt() >= SPREAD
}

/// The box model, in device pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metrics {
    /// The narrowest a column may be: as many columns as fit at this width,
    /// widened to fill the viewport.
    pub column: i32,
    /// The caption line under each tile.
    pub label: i32,
    /// Between columns, and between a caption and the tile under it.
    pub gap: i32,
    /// Between the tiles and the edge of the viewport.
    pub pad: i32,
}

/// A tile: its picture's box, in content coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tile {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Where one entry went: its column, its top and its picture's height.
#[derive(Clone, Copy, Debug)]
struct Placed {
    column: usize,
    y: f32,
    h: f32,
}

/// Every entry's tile, laid out for one width.
#[derive(Debug)]
pub struct Masonry {
    placed: Vec<Placed>,
    /// Each column's entries, top to bottom (and so in ascending order).
    columns: Vec<Vec<usize>>,
    width: f32,
    label: f32,
    gap: f32,
    pad: f32,
    /// The tallest picture, which bounds how far above a point a tile that
    /// still reaches it can start.
    tallest: f32,
    /// The whole height, padding included.
    pub content: f32,
}

impl Masonry {
    /// Lay out entries of `ratios` (width over height, one per entry) across a
    /// viewport `width` wide.
    pub fn build(ratios: &[f32], width: i32, m: Metrics) -> Masonry {
        let pad = m.pad as f32;
        let gap = m.gap as f32;
        let label = m.label as f32;
        let avail = (width - 2 * m.pad).max(1) as f32;
        let min_column = m.column.max(1) as f32;
        let count = (((avail + gap) / (min_column + gap)).floor() as usize).max(1);
        let column_w = ((avail - gap * (count - 1) as f32) / count as f32).max(1.0);

        let mut heights = vec![pad; count];
        let mut columns = vec![Vec::new(); count];
        let mut placed = Vec::with_capacity(ratios.len());
        let mut tallest: f32 = 0.0;
        for (i, ratio) in ratios.iter().enumerate() {
            let ratio = Some(*ratio)
                .filter(|r| r.is_finite() && *r > 0.0)
                .unwrap_or(1.0)
                .clamp(MIN_RATIO, MAX_RATIO);
            let h = column_w / ratio;
            // The shortest column, the leftmost of any tie, so a first row
            // fills left to right.
            let column = heights
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.total_cmp(b.1))
                .map_or(0, |(c, _)| c);
            let y = heights.get(column).copied().unwrap_or(pad);
            placed.push(Placed { column, y, h });
            if let Some(list) = columns.get_mut(column) {
                list.push(i);
            }
            if let Some(height) = heights.get_mut(column) {
                *height = y + h + label + gap;
            }
            tallest = tallest.max(h);
        }
        let content = if placed.is_empty() {
            2.0 * pad
        } else {
            heights.iter().copied().fold(pad, f32::max) - gap + pad
        };
        Masonry {
            placed,
            columns,
            width: column_w,
            label,
            gap,
            pad,
            tallest,
            content,
        }
    }

    /// How many entries were laid out.
    pub fn len(&self) -> usize {
        self.placed.len()
    }

    /// The left edge of column `c`.
    fn column_x(&self, c: usize) -> f32 {
        self.pad + c as f32 * (self.width + self.gap)
    }

    /// Entry `i`'s tile, if there is an entry `i`.
    pub fn tile(&self, i: usize) -> Option<Tile> {
        let p = self.placed.get(i)?;
        Some(Tile {
            x: self.column_x(p.column),
            y: p.y,
            w: self.width,
            h: p.h,
        })
    }

    /// The entries whose top lies strictly between `top` and `bottom`.
    pub fn between(&self, top: f32, bottom: f32) -> Range<usize> {
        let start = self.placed.partition_point(|p| p.y <= top);
        let end = self.placed.partition_point(|p| p.y < bottom);
        start..end.max(start)
    }

    /// The entries any part of which, caption included, may lie between
    /// `top` and `bottom`: every one that does, and a few just above that are
    /// shorter than the tallest and do not.
    pub fn overlapping(&self, top: f32, bottom: f32) -> Range<usize> {
        let reach = self.tallest + self.label;
        let start = self.placed.partition_point(|p| p.y + reach <= top);
        let end = self.placed.partition_point(|p| p.y < bottom);
        start..end.max(start)
    }

    /// The column at content `x`, if `x` is on one rather than in a gap.
    fn column_at(&self, px: f32) -> Option<usize> {
        if px < self.pad {
            return None;
        }
        let c = ((px - self.pad) / (self.width + self.gap)).floor() as usize;
        (c < self.columns.len() && px < self.column_x(c) + self.width).then_some(c)
    }

    /// The entry whose tile or caption is at `(px, py)`, if one is.
    pub fn hit(&self, px: f32, py: f32) -> Option<usize> {
        let list = self.columns.get(self.column_at(px)?)?;
        let at = list
            .partition_point(|&i| self.placed.get(i).is_some_and(|p| p.y <= py))
            .checked_sub(1)?;
        let i = *list.get(at)?;
        let p = self.placed.get(i)?;
        (py < p.y + p.h + self.label).then_some(i)
    }

    /// Entry `i`'s place in its column: the column's list and where in it.
    fn in_column(&self, i: usize) -> Option<(&Vec<usize>, usize)> {
        let list = self.columns.get(self.placed.get(i)?.column)?;
        Some((list, list.binary_search(&i).ok()?))
    }

    /// The entry `steps` above (negative) or below entry `from` in its
    /// column, which is where the arrow keys go; it stops at the column's
    /// first and last.
    pub fn vertical(&self, from: usize, steps: i32) -> usize {
        let Some((list, at)) = self.in_column(from) else {
            return from;
        };
        let to = (at as i64 + steps as i64).clamp(0, list.len() as i64 - 1) as usize;
        list.get(to).copied().unwrap_or(from)
    }

    /// The entry in the column `dir` to the left (negative) or right of entry
    /// `from`'s whose middle is level with `from`'s, or nearest it. From the
    /// first or last column, the entry before or after in the listing, as
    /// the square grid's Left and Right step on along it.
    pub fn horizontal(&self, from: usize, dir: i32) -> usize {
        let last = self.len().saturating_sub(1);
        let Some(p) = self.placed.get(from) else {
            return from;
        };
        let along = || (from as i64 + dir as i64).clamp(0, last as i64) as usize;
        let column = p.column as i64 + dir as i64;
        let Some(list) = usize::try_from(column)
            .ok()
            .and_then(|c| self.columns.get(c))
            .filter(|l| !l.is_empty())
        else {
            return along();
        };
        let middle = |i: usize| self.placed.get(i).map_or(f32::MAX, |q| q.y + q.h / 2.0);
        let level = middle(from);
        list.iter()
            .copied()
            .min_by(|&a, &b| {
                (middle(a) - level)
                    .abs()
                    .total_cmp(&(middle(b) - level).abs())
            })
            .unwrap_or(from)
    }

    /// The entry in the middle column nearest content height `y`.
    pub fn at_height(&self, y: f32) -> usize {
        let Some(list) = self.columns.get(self.columns.len() / 2) else {
            return 0;
        };
        let at = list
            .partition_point(|&i| self.placed.get(i).is_some_and(|p| p.y <= y))
            .saturating_sub(1);
        list.get(at).copied().unwrap_or(0)
    }

    /// The typical distance from one tile's top to the next one down a
    /// column: a 4:3 picture, its caption and the gap. What a page of rows is
    /// counted in.
    pub fn pitch(&self) -> f32 {
        self.width * 0.75 + self.label + self.gap
    }
}

#[cfg(test)]
mod tests {
    use crate::masonry::*;

    fn metrics() -> Metrics {
        Metrics {
            column: 100,
            label: 20,
            gap: 10,
            pad: 10,
        }
    }

    #[test]
    fn columns_fill_the_width_at_one_width() {
        // 450 wide: 430 between the pads holds four columns of 100 and three
        // gaps, widened to (430 - 30) / 4 = 100 exactly; 460 widens them.
        let m = Masonry::build(&[1.0; 8], 450, metrics());
        assert_eq!(m.columns.len(), 4);
        let m = Masonry::build(&[1.0; 8], 470, metrics());
        assert_eq!(m.columns.len(), 4);
        let t = m.tile(3).unwrap();
        assert!(
            (t.x + t.w - 460.0).abs() < 0.01,
            "the last column ends at the pad"
        );
        assert!((0..8).all(|i| m.tile(i).unwrap().w == t.w));
    }

    #[test]
    fn each_picture_keeps_its_shape_under_the_shortest_column() {
        // Three columns of 100. A tall one, a wide one, a square: the next
        // goes under the wide one, the shortest.
        let m = Masonry::build(&[0.5, 2.0, 1.0, 1.0], 340, metrics());
        assert_eq!(m.columns.len(), 3);
        let [a, b, c, d] = [0, 1, 2, 3].map(|i| m.tile(i).unwrap());
        assert_eq!((a.h, b.h, c.h), (200.0, 50.0, 100.0), "height from shape");
        assert_eq!(
            (a.y, b.y, c.y),
            (10.0, 10.0, 10.0),
            "the first row along the top"
        );
        assert_eq!(d.x, b.x, "under the shortest column");
        assert_eq!(
            d.y,
            b.y + b.h + 20.0 + 10.0,
            "below its caption and the gap"
        );
        assert_eq!(m.content, a.y + a.h + 20.0 + 10.0, "the tallest column");
    }

    #[test]
    fn tops_come_in_the_listing_order() {
        let ratios: Vec<f32> = (0..300)
            .map(|i| [1.5, 0.67, 1.0, 1.78, 0.4][i % 5])
            .collect();
        let m = Masonry::build(&ratios, 800, metrics());
        for i in 1..m.len() {
            assert!(
                m.tile(i).unwrap().y >= m.tile(i - 1).unwrap().y,
                "entry {i}"
            );
        }
    }

    #[test]
    fn extreme_shapes_are_capped() {
        let m = Masonry::build(&[20.0, 0.01], 340, metrics());
        assert_eq!(m.tile(0).unwrap().h, 25.0);
        assert_eq!(m.tile(1).unwrap().h, 400.0);
    }

    #[test]
    fn nothing_laid_out_is_only_padding() {
        let m = Masonry::build(&[], 600, metrics());
        assert_eq!(m.content, 20.0);
        assert_eq!(m.tile(0), None);
        assert_eq!(m.hit(50.0, 50.0), None);
        assert_eq!(m.between(-1000.0, 1000.0), 0..0);
        assert_eq!(m.overlapping(-1000.0, 1000.0), 0..0);
        assert_eq!(m.vertical(0, 1), 0);
        assert_eq!(m.horizontal(0, 1), 0);
        assert_eq!(m.at_height(100.0), 0);
    }

    #[test]
    fn a_click_lands_on_the_tile_or_caption_it_is_drawn_on() {
        let ratios = [1.5, 0.67, 1.0, 1.5, 0.8, 1.2, 1.78, 0.5, 2.0];
        let m = Masonry::build(&ratios, 450, metrics());
        for i in 0..m.len() {
            let t = m.tile(i).unwrap();
            assert_eq!(m.hit(t.x + 1.0, t.y + 1.0), Some(i));
            assert_eq!(
                m.hit(t.x + t.w - 1.0, t.y + t.h + 19.0),
                Some(i),
                "the caption"
            );
            assert_eq!(m.hit(t.x + t.w + 2.0, t.y + 5.0).filter(|&j| j == i), None);
            assert_eq!(m.hit(t.x + 1.0, t.y + t.h + 25.0).filter(|&j| j == i), None);
        }
        assert_eq!(m.hit(5.0, 50.0), None, "the left pad");
        assert_eq!(m.hit(50.0, 5.0), None, "the top pad");
    }

    #[test]
    fn up_and_down_stay_in_the_column_and_stop_at_its_ends() {
        let ratios: Vec<f32> = (0..30).map(|i| [1.5, 0.67, 1.0][i % 3]).collect();
        let m = Masonry::build(&ratios, 340, metrics());
        for list in &m.columns {
            for (at, &i) in list.iter().enumerate() {
                let below = m.vertical(i, 1);
                assert_eq!(below, list[(at + 1).min(list.len() - 1)]);
                assert_eq!(m.vertical(i, -1), list[at.saturating_sub(1)]);
            }
            assert_eq!(m.vertical(list[0], -5), list[0]);
            assert_eq!(m.vertical(list[0], 99), *list.last().unwrap());
        }
    }

    #[test]
    fn left_and_right_go_to_the_tile_level_with_this_one() {
        let ratios: Vec<f32> = (0..30).map(|i| [1.5, 0.67, 1.0, 0.5][i % 4]).collect();
        let m = Masonry::build(&ratios, 340, metrics());
        let middle = |i: usize| {
            let t = m.tile(i).unwrap();
            t.y + t.h / 2.0
        };
        for &i in &m.columns[1] {
            for dir in [-1, 1] {
                let to = m.horizontal(i, dir);
                let column = &m.columns[(1 + dir) as usize];
                assert!(column.contains(&to));
                for &j in column {
                    assert!((middle(to) - middle(i)).abs() <= (middle(j) - middle(i)).abs());
                }
            }
        }
        // From an edge column, on along the listing.
        let first = m.columns[0][2];
        assert_eq!(m.horizontal(first, -1), first - 1);
        assert_eq!(m.horizontal(0, -1), 0);
    }

    #[test]
    fn between_and_overlapping_agree_with_each_tile() {
        let ratios: Vec<f32> = (0..200)
            .map(|i| [1.5, 0.67, 1.0, 1.78, 0.3][i % 5])
            .collect();
        let m = Masonry::build(&ratios, 800, metrics());
        for (top, bottom) in [
            (-50.0, 300.0),
            (130.0, 900.0),
            (2000.0, 2600.0),
            (-500.0, -10.0),
        ] {
            let between = m.between(top, bottom);
            let overlapping = m.overlapping(top, bottom);
            for i in 0..m.len() {
                let t = m.tile(i).unwrap();
                assert_eq!(between.contains(&i), t.y > top && t.y < bottom, "{i}");
                if t.y < bottom && t.y + t.h + 20.0 > top {
                    assert!(overlapping.contains(&i), "{i} is on screen");
                }
            }
        }
    }

    #[test]
    fn spread_shapes_prefer_masonry_and_one_shape_squares() {
        assert!(!prefers_masonry(&[1.5; 30]), "one camera, landscape");
        assert!(!prefers_masonry(&[1.0, 1.0, 1.05, 0.95]), "icons");
        let mixed: Vec<f32> = (0..30)
            .map(|i| if i % 3 == 0 { 0.67 } else { 1.5 })
            .collect();
        assert!(prefers_masonry(&mixed), "landscape and portrait");
        assert!(prefers_masonry(&[0.5, 1.0, 2.0, 1.3, 0.8, 3.0]));
        assert!(!prefers_masonry(&[0.67]), "one picture says nothing");
        assert!(!prefers_masonry(&[]));
    }

    #[test]
    fn shapes_read_back_and_step_round() {
        for s in Shape::ALL {
            assert_eq!(Shape::from_key(s.key()), Some(s));
        }
        assert_eq!(Shape::Auto.next().next().next(), Shape::Auto);
        assert_eq!(Shape::from_key("justified"), None);
    }
}
