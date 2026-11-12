//! The explorer's geometry and the text its columns show.
//!
//! Pure layout math and pure formatting, the same bargain [`crate::grid`] makes:
//! the painter and the hit tests take their answers from one place, so a click
//! lands on the row it looks like it lands on by construction rather than by two
//! call sites agreeing. Device pixels throughout; the constants are authored in
//! logical pixels and scaled on the way in.
//!
//! Where the sidebar, the header and the content sit in the window is
//! [`crate::regions`]'s answer; this module places rows and columns inside them.
//! The size and modified columns drop out below their own thresholds, so a
//! narrow window degrades to a list of names rather than to three columns too
//! cramped to read.

use std::path::{Path, PathBuf};

use crate::order::SortKey;
use crate::platform::geom::{Rect, Scale};
use crate::platform::grapheme;
use crate::regions::Regions;

/// The padding inside a row, and the inset of the content from its edges.
/// Logical pixels.
pub const ROW_PAD: i32 = 8;
pub const PAD: i32 = 12;
/// The side of an entry's icon in the detail list, and the gap between columns.
pub const ICON: i32 = 16;
pub const GAP: i32 = 12;

/// Below these content widths the two right-hand columns are not worth the room
/// they take. Logical pixels.
const SIZE_MIN_CONTENT: i32 = 300;
const MODIFIED_MIN_CONTENT: i32 = 460;

/// What each column is worth when there is room for it. Logical pixels.
const SIZE_MIN: i32 = 70;
const SIZE_MAX: i32 = 110;
const MODIFIED_MIN: i32 = 124;
const MODIFIED_MAX: i32 = 190;

/// Where the detail list's columns sit, in absolute device pixels. A width of zero
/// means the column did not fit and is not drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Columns {
    pub icon_x: i32,
    pub icon_w: i32,
    pub name_x: i32,
    pub name_w: i32,
    pub size_x: i32,
    pub size_w: i32,
    pub modified_x: i32,
    pub modified_w: i32,
}

impl Columns {
    /// Divide `content` between the icon, the name, and the two right-hand columns.
    ///
    /// The name takes whatever the others leave, so it is the column that grows
    /// with the window; size and modified take a share of the width clamped to a
    /// readable band, so they neither crush at 800px nor sprawl at 3440.
    fn new(scale: Scale, content: Rect) -> Self {
        let pad = scale.px(PAD);
        let gap = scale.px(GAP);
        let icon_w = scale.px(ICON);
        let avail = (content.w - 2 * pad).max(0);

        let modified_w = if avail >= scale.px(MODIFIED_MIN_CONTENT) {
            (avail / 4).clamp(scale.px(MODIFIED_MIN), scale.px(MODIFIED_MAX))
        } else {
            0
        };
        let size_w = if avail >= scale.px(SIZE_MIN_CONTENT) {
            (avail / 8).clamp(scale.px(SIZE_MIN), scale.px(SIZE_MAX))
        } else {
            0
        };
        let taken = icon_w
            + gap
            + if size_w > 0 { size_w + gap } else { 0 }
            + if modified_w > 0 { modified_w + gap } else { 0 };
        let name_w = (avail - taken).max(scale.px(40));

        let icon_x = content.x + pad;
        let name_x = icon_x + icon_w + gap;
        let size_x = name_x + name_w + gap;
        let modified_x = size_x + if size_w > 0 { size_w + gap } else { 0 };
        Self {
            icon_x,
            icon_w,
            name_x,
            name_w,
            size_x,
            size_w,
            modified_x,
            modified_w,
        }
    }
}

/// Where the rows and columns land for one set of regions, entry count, and
/// scroll offset.
pub struct ExplorerLayout {
    /// The places sidebar. Width zero when it is not shown.
    pub sidebar: Rect,
    /// The detail list's column headings, a band at the top of the content
    /// region that stays put while the rows scroll. Zero tall when there is
    /// none, as in the grid.
    pub heading: Rect,
    /// Where entries are drawn: the content region below the headings.
    pub content: Rect,
    /// Distance between row origins, in the list and in the sidebar alike.
    pub row: i32,
    pub scroll: f32,
    pub max_scroll: f32,
    /// The height of the whole list, scrolled or not: what a scrollbar measures its
    /// thumb against.
    pub content_height: i32,
    pub columns: Columns,
    /// Inset of the first row from the top of its region.
    pad: i32,
}

impl ExplorerLayout {
    /// Lay `count` entries out in `regions` at the desired `scroll` (clamped
    /// here), under a row of column headings if `headings` is set.
    pub fn new(
        scale: Scale,
        regions: &Regions,
        line_height: i32,
        count: usize,
        scroll: f32,
        headings: bool,
    ) -> Self {
        let pad = scale.px(ROW_PAD);
        let row = (line_height + pad).max(1);
        let region = regions.content;
        let heading_h = if headings { row.min(region.h) } else { 0 };
        let heading = Rect {
            h: heading_h,
            ..region
        };
        let content = Rect {
            y: region.y + heading_h,
            h: region.h - heading_h,
            ..region
        };
        let content_height = 2 * pad + count as i32 * row;
        let max_scroll = (content_height - content.h).max(0) as f32;
        Self {
            sidebar: regions.sidebar,
            heading,
            content,
            row,
            scroll: scroll.clamp(0.0, max_scroll),
            max_scroll,
            content_height,
            columns: Columns::new(scale, content),
            pad,
        }
    }

    /// Top of row `i` in window coordinates, scroll already applied.
    pub fn row_y(&self, i: usize) -> f32 {
        (self.content.y + self.pad) as f32 + i as f32 * self.row as f32 - self.scroll
    }

    /// The row at window point `(px, py)`, if it lands on one. A point outside the
    /// content region — in the sidebar, the header, the status bar — hits nothing.
    pub fn row_at(&self, count: usize, px: f32, py: f32) -> Option<usize> {
        if !contains(self.content, px, py) {
            return None;
        }
        let rel = py - (self.content.y + self.pad) as f32 + self.scroll;
        if rel < 0.0 {
            return None;
        }
        let i = (rel / self.row as f32) as usize;
        (i < count).then_some(i)
    }

    /// The column heading at window point `(px, py)`, if it lands on one. Each
    /// heading spans its column; the name's takes in the icon before it.
    pub fn heading_at(&self, px: f32, py: f32) -> Option<SortKey> {
        if !contains(self.heading, px, py) {
            return None;
        }
        let c = self.columns;
        let span = |x: i32, w: i32| w > 0 && px >= x as f32 && px < (x + w) as f32;
        if span(c.icon_x, c.name_x + c.name_w - c.icon_x) {
            Some(SortKey::Name)
        } else if span(c.size_x, c.size_w) {
            Some(SortKey::Size)
        } else if span(c.modified_x, c.modified_w) {
            Some(SortKey::Modified)
        } else {
            None
        }
    }

    /// The scroll that brings row `i` fully into view, moving as little as possible.
    pub fn reveal(&self, i: usize) -> f32 {
        let top = self.pad + i as i32 * self.row;
        crate::platform::scroll::reveal(
            self.scroll as i32,
            top,
            self.row,
            self.content.h,
            self.content_height,
            self.pad,
        ) as f32
    }

    /// Top of sidebar row `i`. The sidebar does not scroll: it holds a handful of
    /// places, and one that does not fit is one the window is too short for.
    pub fn place_y(&self, i: usize) -> i32 {
        self.sidebar.y + self.pad + i as i32 * self.row
    }

    /// The sidebar place at window point `(px, py)`, if it lands on one.
    pub fn place_at(&self, count: usize, px: f32, py: f32) -> Option<usize> {
        if self.sidebar.w == 0 || !contains(self.sidebar, px, py) {
            return None;
        }
        let rel = py - (self.sidebar.y + self.pad) as f32;
        if rel < 0.0 {
            return None;
        }
        let i = (rel / self.row as f32) as usize;
        (i < count).then_some(i)
    }
}

/// Whether `(px, py)` lands inside `rect`, with the right and bottom edges
/// exclusive so two rectangles that touch cannot both claim a point.
pub fn contains(rect: Rect, px: f32, py: f32) -> bool {
    px >= rect.x as f32
        && px < (rect.x + rect.w) as f32
        && py >= rect.y as f32
        && py < (rect.y + rect.h) as f32
}

// ---- the breadcrumb ------------------------------------------------------

/// One clickable run of the breadcrumb: what it reads, where it goes, and where it
/// was drawn so a click can be matched back to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub label: String,
    pub target: PathBuf,
    pub rect: Rect,
}

/// The separator drawn between breadcrumb segments, and the stand-in for the segments
/// that did not fit.
pub const SEPARATOR: &str = "›";
const ELLIPSIS: &str = "…";

/// Lay the current directory out as clickable segments inside `band`.
///
/// Truncates from the *left*: where you are matters more than that you are
/// somewhere under `/home`, so a path too long for the window drops its leading
/// segments and stands an ellipsis in for them. The ellipsis is itself clickable
/// and goes to the parent of the first segment still shown, so walking back up a
/// deep path never needs the window widened.
pub fn path_segments(
    dir: &Path,
    band: Rect,
    scale: Scale,
    measure: &dyn Fn(&str) -> i32,
) -> Vec<Segment> {
    // Every prefix of the path, root first, as (what it reads, where it goes).
    let mut parts: Vec<(String, PathBuf)> = Vec::new();
    let mut walked = PathBuf::new();
    for component in dir.components() {
        walked.push(component.as_os_str());
        let label = match component {
            std::path::Component::RootDir => "/".to_string(),
            other => other.as_os_str().to_string_lossy().into_owned(),
        };
        parts.push((label, walked.clone()));
    }
    if parts.is_empty() {
        return Vec::new();
    }

    let pad = scale.px(PAD);
    let gap = scale.px(GAP);
    let sep_w = measure(SEPARATOR) + 2 * gap;
    let avail = (band.w - 2 * pad).max(0);

    // How many trailing segments fit. Counted from the end, because that is the end
    // that is kept; the ellipsis is only paid for when something is actually
    // dropped, which is why the budget is recomputed inside the loop.
    let widths: Vec<i32> = parts.iter().map(|(label, _)| measure(label)).collect();
    let mut kept = 0usize;
    let mut used = 0i32;
    for i in (0..parts.len()).rev() {
        let sep = if kept > 0 { sep_w } else { 0 };
        let elision = if i > 0 { measure(ELLIPSIS) + sep_w } else { 0 };
        if used + sep + widths[i] + elision > avail && kept > 0 {
            break;
        }
        used += sep + widths[i];
        kept += 1;
    }
    let first = parts.len() - kept;

    let mut out = Vec::new();
    let mut x = band.x + pad;
    let mut place = |label: String, target: PathBuf, width: i32, x: &mut i32| {
        out.push(Segment {
            label,
            target,
            rect: Rect {
                x: *x,
                y: band.y,
                w: width,
                h: band.h,
            },
        });
        *x += width;
    };
    if first > 0 {
        // The dropped head, as one segment that goes to the parent of what is left.
        let target = parts[first - 1].1.clone();
        place(ELLIPSIS.to_string(), target, measure(ELLIPSIS), &mut x);
        x += sep_w;
    }
    for (i, (label, target)) in parts.iter().enumerate().skip(first) {
        if i > first {
            x += sep_w;
        }
        place(label.clone(), target.clone(), widths[i], &mut x);
    }
    out
}

/// The segment at window point `(px, py)`, if the click landed on one.
pub fn segment_at(segments: &[Segment], px: f32, py: f32) -> Option<usize> {
    segments.iter().position(|s| contains(s.rect, px, py))
}

// ---- column text ---------------------------------------------------------

/// A file's size, the way a file manager says it: powers of 1024 under the units
/// people actually speak, one decimal only where it carries information.
pub fn format_size(bytes: u64) -> String {
    // Through EB, so that no real file size can saturate the table: a u64 of bytes
    // tops out at 16 EB, which is the largest number this can be asked to say.
    const UNITS: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    // 1048575 bytes is 1023.999 KB, which would round to a nonsensical "1024 KB".
    if unit + 1 < UNITS.len() && value.round() >= 1024.0 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// A timestamp already shifted into the viewer's zone, as `YYYY-MM-DD HH:MM`.
///
/// Sortable, unambiguous, and the same width for every entry, which is what a
/// column of them wants. Seconds are dropped: a file listing is read at a glance
/// and a column two characters narrower fits more of the name beside it.
pub fn format_stamp(local_secs: i64) -> String {
    let (y, m, d, hour, min, _) = crate::localtime::civil_from_unix(local_secs);
    format!("{y:04}-{m:02}-{d:02} {hour:02}:{min:02}")
}

/// Fit `text` into `max_w` by taking characters out of the middle.
///
/// The middle, not the end: file names in a directory share their beginnings
/// (`IMG_20260714_`) and carry their meaning in what follows, and truncating on the
/// right leaves a column of identical stems. Keeping both ends also keeps the
/// extension visible, which is half of what the name says.
pub fn truncate_middle(text: &str, max_w: i32, measure: &dyn Fn(&str) -> i32) -> String {
    if max_w <= 0 {
        return String::new();
    }
    if measure(text) <= max_w {
        return text.to_string();
    }
    // Grapheme clusters, not chars: cutting a flag or a skin-toned emoji in half
    // would still be valid UTF-8 and would still render as garbage.
    let clusters: Vec<&str> = grapheme::graphemes(text)
        .map(|(_, cluster)| cluster)
        .collect();
    // Binary search the largest number of clusters that fits. Width grows with the
    // count, so the fit is monotone and the search is exact.
    let (mut lo, mut hi) = (0usize, clusters.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&elide(&clusters, mid)) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    elide(&clusters, lo)
}

/// `keep` clusters of `text`, split around an ellipsis. The head takes the odd one:
/// a name is identified by how it starts more than by how it ends.
fn elide(clusters: &[&str], keep: usize) -> String {
    if keep >= clusters.len() {
        return clusters.concat();
    }
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut out = clusters[..head].concat();
    out.push_str(ELLIPSIS);
    out.push_str(&clusters[clusters.len() - tail..].concat());
    out
}

#[cfg(test)]
mod tests {
    use crate::explorer::*;
    use crate::regions::{Panels, HEADER_H, SIDEBAR_W};

    /// A fake text measurer: every character is 10 wide. Lets the layout tests pin
    /// exact numbers without opening a font.
    fn measure(s: &str) -> i32 {
        s.chars().count() as i32 * 10
    }

    fn layout(w: i32, h: i32, count: usize, scroll: f32) -> ExplorerLayout {
        // 20px lines, a 30px status bar, at unity scale, sidebar and header shown.
        let panels = Panels {
            sidebar: true,
            header: true,
            inspector: false,
        };
        let regions = Regions::new(Scale::ONE, w, h, 30, panels);
        ExplorerLayout::new(Scale::ONE, &regions, 20, count, scroll, false)
    }

    #[test]
    fn the_headings_sit_over_the_rows_and_name_their_columns() {
        let panels = Panels {
            sidebar: true,
            header: true,
            inspector: false,
        };
        let regions = Regions::new(Scale::ONE, 1000, 600, 30, panels);
        let l = ExplorerLayout::new(Scale::ONE, &regions, 20, 40, 0.0, true);
        // A band one row tall at the top of the content region; the rows start
        // under it and scroll in what is left.
        assert_eq!(l.heading.y, regions.content.y);
        assert_eq!(l.heading.h, 28);
        assert_eq!(l.content.y, regions.content.y + 28);
        assert_eq!(l.content.h, regions.content.h - 28);
        assert_eq!(l.row_y(0), (l.content.y + ROW_PAD) as f32);
        assert_eq!(l.max_scroll, (2 * 8 + 40 * 28 - (526 - 28)) as f32);
        // A click on the headings hits a heading, never a row.
        let y = (l.heading.y + 10) as f32;
        let c = l.columns;
        assert_eq!(l.heading_at((c.name_x + 5) as f32, y), Some(SortKey::Name));
        assert_eq!(l.heading_at((c.icon_x + 2) as f32, y), Some(SortKey::Name));
        assert_eq!(l.heading_at((c.size_x + 5) as f32, y), Some(SortKey::Size));
        assert_eq!(
            l.heading_at((c.modified_x + 5) as f32, y),
            Some(SortKey::Modified)
        );
        assert_eq!(l.row_at(40, (c.name_x + 5) as f32, y), None);
        // Below the band is a row, not a heading.
        let below = (l.content.y + ROW_PAD + 2) as f32;
        assert_eq!(l.heading_at((c.name_x + 5) as f32, below), None);
        assert_eq!(l.row_at(40, (c.name_x + 5) as f32, below), Some(0));
        // Without headings there is no band and nothing to hit.
        let bare = ExplorerLayout::new(Scale::ONE, &regions, 20, 40, 0.0, false);
        assert_eq!((bare.heading.h, bare.content), (0, regions.content));
        assert_eq!(bare.heading_at((c.name_x + 5) as f32, y), None);
    }

    #[test]
    fn columns_drop_from_the_right_as_the_window_narrows() {
        // Wide: all three, laid out left to right without overlapping.
        let c = layout(1400, 600, 10, 0.0).columns;
        assert!(c.size_w > 0 && c.modified_w > 0);
        assert!(c.icon_x < c.name_x);
        assert!(c.name_x + c.name_w <= c.size_x);
        assert!(c.size_x + c.size_w <= c.modified_x);

        // Narrow enough to lose modified, then narrow enough to lose size too. The
        // name column survives everything: a listing with no names is not a listing.
        let c = layout(400, 600, 10, 0.0).columns;
        assert_eq!(c.modified_w, 0);
        assert!(c.size_w > 0, "size still fits at 400");
        let c = layout(240, 600, 10, 0.0).columns;
        assert_eq!((c.size_w, c.modified_w), (0, 0));
        assert!(c.name_w > 0);
    }

    #[test]
    fn rows_stack_below_the_header_and_scroll_clamps_to_the_list() {
        let l = layout(1000, 600, 10, 0.0);
        assert_eq!(l.row, 28); // 20px line + 8px padding
        assert_eq!(l.row_y(0), (HEADER_H + ROW_PAD) as f32);
        assert_eq!(l.row_y(1), (HEADER_H + ROW_PAD + 28) as f32);

        // content region is 600 - 44 - 30 = 526 tall; 10 rows is 2*8 + 280 = 296,
        // which fits, so there is nowhere to scroll to.
        assert_eq!(l.max_scroll, 0.0);
        assert_eq!(layout(1000, 600, 10, 99.0).scroll, 0.0);

        // 40 rows is 1136, so 610 of travel, and a scroll past it clamps.
        let tall = layout(1000, 600, 40, 100_000.0);
        assert_eq!(tall.content_height, 2 * 8 + 40 * 28);
        assert_eq!(tall.max_scroll, (2 * 8 + 40 * 28 - 526) as f32);
        assert_eq!(tall.scroll, tall.max_scroll);
    }

    #[test]
    fn a_click_hits_the_row_it_looks_like_it_hits() {
        let l = layout(1000, 600, 10, 0.0);
        let x = (SIDEBAR_W + 50) as f32;
        assert_eq!(l.row_at(10, x, l.row_y(0)), Some(0));
        assert_eq!(l.row_at(10, x, l.row_y(0) + 27.0), Some(0));
        assert_eq!(l.row_at(10, x, l.row_y(1)), Some(1));
        assert_eq!(l.row_at(10, x, 4000.0), None, "past the last row");
        // Outside the content band: the header, the sidebar, the status bar.
        assert_eq!(l.row_at(10, x, 10.0), None, "in the header");
        assert_eq!(l.row_at(10, 20.0, l.row_y(0)), None, "in the sidebar");
        assert_eq!(l.row_at(10, x, 590.0), None, "in the status bar");
    }

    #[test]
    fn a_click_hits_the_right_row_when_the_list_is_scrolled() {
        let l = layout(1000, 600, 40, 56.0); // scrolled by two rows
        let x = (SIDEBAR_W + 50) as f32;
        let top = (HEADER_H + ROW_PAD) as f32;
        assert_eq!(l.row_y(2), top);
        assert_eq!(l.row_at(40, x, top), Some(2));
    }

    #[test]
    fn revealing_a_row_below_the_fold_scrolls_it_just_into_view() {
        let l = layout(1000, 600, 40, 0.0);
        let scrolled = l.reveal(30);
        assert!(scrolled > 0.0, "row 30 is below the fold");
        let after = layout(1000, 600, 40, scrolled);
        let y = after.row_y(30);
        assert!(y >= after.content.y as f32);
        assert!(y + after.row as f32 <= (after.content.y + after.content.h) as f32);
        // A row already in view does not move the list.
        assert_eq!(after.reveal(30), scrolled);
    }

    #[test]
    fn the_sidebar_hit_test_ignores_the_list_and_vice_versa() {
        let l = layout(1000, 600, 10, 0.0);
        let y = l.place_y(1) as f32 + 2.0;
        assert_eq!(l.place_at(5, 40.0, y), Some(1));
        assert_eq!(l.place_at(5, (SIDEBAR_W + 40) as f32, y), None);
        assert_eq!(l.place_at(5, 40.0, 2.0), None, "above the first place");
        assert_eq!(l.place_at(1, 40.0, y), None, "past the last place");
        assert_eq!(l.place_at(5, 40.0, 590.0), None, "the status bar");
        // With no sidebar there is nothing to hit at all.
        let narrow = layout(400, 600, 10, 0.0);
        assert_eq!(narrow.place_at(5, 10.0, y), None);
    }

    // ---- the breadcrumb --------------------------------------------------

    fn band(w: i32) -> Rect {
        Rect {
            x: 0,
            y: 0,
            w,
            h: HEADER_H,
        }
    }

    #[test]
    fn the_path_becomes_one_segment_per_component() {
        let segments = path_segments(Path::new("/home/me/pics"), band(2000), Scale::ONE, &measure);
        let labels: Vec<&str> = segments.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, vec!["/", "home", "me", "pics"]);
        // Each goes to the prefix it names, so clicking "home" leaves /home/me/pics
        // for /home rather than for a directory called "home" underneath it.
        let targets: Vec<&Path> = segments.iter().map(|s| s.target.as_path()).collect();
        assert_eq!(
            targets,
            vec![
                Path::new("/"),
                Path::new("/home"),
                Path::new("/home/me"),
                Path::new("/home/me/pics")
            ]
        );
        // Laid out left to right without overlapping.
        for pair in segments.windows(2) {
            assert!(pair[0].rect.x + pair[0].rect.w <= pair[1].rect.x);
        }
    }

    #[test]
    fn a_path_too_long_for_the_window_drops_its_head() {
        let deep = Path::new("/home/me/projects/bnkfiles/src/platform/very/deep/leaf");
        let segments = path_segments(deep, band(300), Scale::ONE, &measure);
        let labels: Vec<&str> = segments.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels[0], ELLIPSIS, "the dropped head is marked");
        assert_eq!(
            labels.last().copied(),
            Some("leaf"),
            "where you are is what survives"
        );
        // The ellipsis is clickable and goes up, so a deep path is still walkable
        // without widening the window.
        assert!(
            segments[0].target.ends_with("deep") || segments[0].target.components().count() > 1
        );
        // Everything drawn stays inside the band.
        let right = segments.last().expect("a segment").rect;
        assert!(right.x + right.w <= 300, "segments overflow the band");
    }

    #[test]
    fn segments_start_inside_a_header_that_does_not_start_at_the_window_edge() {
        // The header sits to the right of the sidebar, so the breadcrumb must lay
        // itself out from the band's own origin, not from the window's.
        let header = Rect {
            x: 190,
            y: 0,
            w: 800,
            h: HEADER_H,
        };
        let segments = path_segments(Path::new("/home/me"), header, Scale::ONE, &measure);
        assert_eq!(segments[0].rect.x, 190 + PAD);
        for s in &segments {
            assert!(s.rect.x >= header.x && s.rect.x + s.rect.w <= header.x + header.w);
        }
        // A click at the left of the window, under the sidebar, is not on the path.
        assert_eq!(segment_at(&segments, 5.0, 10.0), None);
    }

    #[test]
    fn a_path_that_just_fits_is_not_elided() {
        // "/" + "ab" with one separator: 10 + 20 + (10 + 24) = 64, plus 24 padding.
        let segments = path_segments(Path::new("/ab"), band(200), Scale::ONE, &measure);
        let labels: Vec<&str> = segments.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, vec!["/", "ab"]);
    }

    #[test]
    fn even_an_impossibly_narrow_band_keeps_the_last_segment() {
        // Where you are is never dropped: a band too narrow for anything still
        // shows the directory you are in rather than an empty bar.
        let segments = path_segments(Path::new("/home/me/pics"), band(10), Scale::ONE, &measure);
        assert_eq!(
            segments.last().map(|s| s.label.as_str()),
            Some("pics"),
            "got {segments:?}"
        );
    }

    #[test]
    fn a_click_in_the_breadcrumb_finds_its_segment() {
        let segments = path_segments(Path::new("/home/me"), band(2000), Scale::ONE, &measure);
        let home = &segments[1];
        let hit = segment_at(
            &segments,
            (home.rect.x + 2) as f32,
            (home.rect.y + 2) as f32,
        );
        assert_eq!(hit, Some(1));
        // The gap between segments belongs to neither.
        assert_eq!(segment_at(&segments, 5000.0, 10.0), None);
    }

    // ---- column text -----------------------------------------------------

    #[test]
    fn sizes_read_the_way_a_file_manager_says_them() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(10 * 1024), "10 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(25 * 1024 * 1024), "25 MB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GB");
        // The carry: 1048575 is 1023.999 KB, which must not print as "1024 KB".
        assert_eq!(format_size(1024 * 1024 - 1), "1.0 MB");
        // The largest number of bytes a u64 can hold still lands inside the table
        // rather than saturating into a five-digit count of the last unit.
        assert_eq!(format_size(u64::MAX), "16 EB");
    }

    #[test]
    fn timestamps_format_as_a_sortable_local_date() {
        // The column's own format. The calendar underneath it is pinned in
        // `localtime`; what this holds is that the column shows minutes, drops
        // seconds, and pads every field to a fixed width.
        assert_eq!(format_stamp(0), "1970-01-01 00:00");
        assert_eq!(format_stamp(1_000_000_000), "2001-09-09 01:46");
        assert_eq!(format_stamp(1_709_164_800), "2024-02-29 00:00");
        assert_eq!(format_stamp(-1), "1969-12-31 23:59");
    }

    #[test]
    fn a_long_name_loses_its_middle_and_keeps_both_ends() {
        let name = "IMG_20260714_holiday_beach_sunset.jpeg";
        let short = truncate_middle(name, 200, &measure);
        assert!(measure(&short) <= 200);
        assert!(short.contains(ELLIPSIS));
        assert!(short.starts_with("IMG_"), "the stem survives: {short}");
        assert!(short.ends_with("jpeg"), "the extension survives: {short}");

        // A name that fits is returned untouched, ellipsis-free.
        assert_eq!(truncate_middle("short.png", 500, &measure), "short.png");
        // And a column with no room at all draws nothing rather than overflowing.
        assert_eq!(truncate_middle(name, 0, &measure), "");
    }

    #[test]
    fn truncation_never_splits_a_grapheme_cluster() {
        // A family emoji is one cluster of several code points; cutting inside it
        // is valid UTF-8 and renders as rubble.
        let name = "photo-👨‍👩‍👧‍👦-family-reunion-2026.jpg";
        for width in (0..400).step_by(10) {
            let out = truncate_middle(name, width, &measure);
            assert!(measure(&out) <= width.max(0), "{out:?} overflows {width}");
            // What came out is a prefix of the name, the ellipsis, and a suffix of
            // it — so every cluster kept is a whole cluster of the original.
            let Some((head, tail)) = out.split_once(ELLIPSIS) else {
                // No ellipsis: either it all fitted, or there was no room for
                // anything at all and the column draws nothing.
                assert!(
                    out == name || out.is_empty(),
                    "un-elided output {out:?} is neither the name nor empty"
                );
                continue;
            };
            assert!(
                name.starts_with(head),
                "{head:?} is not a prefix of the name"
            );
            assert!(name.ends_with(tail), "{tail:?} is not a suffix of the name");
            // And the two ends never overlap into claiming more than there is.
            assert!(head.len() + tail.len() <= name.len());
        }
    }
}
