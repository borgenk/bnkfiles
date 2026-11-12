//! The window's regions: one pure function of the window size and the panel
//! toggles, so every painter and every hit test measures against the same
//! rectangles.
//!
//! ```text
//!   ┌────────────┬────────────────────────────────┬─────────────┐
//!   │ sidebar    │ header  ← → ↑  a › b › c   ▦ ◨ │ inspector   │  HEADER_H
//!   │            ├────────────────────────────────┤             │
//!   │            │ content                        │             │
//!   │            │   list, grid, or the viewer    │             │
//!   │            │                                │             │
//!   ├────────────┴────────────────────────────────┴─────────────┤
//!   │ status                                                    │  status_h
//!   └───────────────────────────────────────────────────────────┘
//!     SIDEBAR_W           the rest                 INSPECTOR_W
//! ```
//!
//! The header belongs to the content column, not to the window: it sits over
//! the content, and the sidebar and the inspector run the full height beside
//! both. The status bar is the only full-width band.
//!
//! A narrow window drops the inspector first, as soon as taking it would leave
//! the content column narrower than it is when the sidebar itself drops; the
//! sidebar goes below [`SIDEBAR_MIN_WINDOW`]. Device pixels throughout; the constants are
//! logical and scaled on the way in.

use crate::platform::geom::{Rect, Scale};

/// The header's height and the sidebar's width. Logical pixels.
pub const HEADER_H: i32 = 44;
pub const SIDEBAR_W: i32 = 190;
/// Below this window width the sidebar is not worth the room it takes, whatever
/// its toggle says. Logical pixels.
pub const SIDEBAR_MIN_WINDOW: i32 = 560;
/// The inspector's width, and the narrowest content column it may leave
/// behind: the column a window just wide enough for the sidebar has. Logical
/// pixels.
pub const INSPECTOR_W: i32 = 280;
const CONTENT_MIN: i32 = SIDEBAR_MIN_WINDOW - SIDEBAR_W;

/// Which optional regions the state on screen asks for. A region asked for may
/// still be dropped when the window is too small for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Panels {
    pub sidebar: bool,
    pub header: bool,
    pub inspector: bool,
}

/// Where every region lands for one window size. A region that is not shown has
/// zero width or zero height, at the position it would have had, so a hit test
/// against it simply finds nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Regions {
    pub sidebar: Rect,
    pub header: Rect,
    pub content: Rect,
    pub inspector: Rect,
    pub status: Rect,
}

impl Regions {
    /// Lay out a window `w` x `h` device pixels with a status bar `status_h`
    /// tall along the bottom.
    ///
    /// The status bar is reserved first, and the content keeps at least one
    /// pixel whatever that leaves, so a window shorter than its chrome still has a canvas to lay out
    /// against rather than a negative one.
    pub fn new(scale: Scale, w: i32, h: i32, status_h: i32, panels: Panels) -> Self {
        let w = w.max(1);
        let h = h.max(1);
        let status_h = status_h.clamp(0, h - 1);
        let body_h = h - status_h;

        let sidebar_w = if panels.sidebar && w >= scale.px(SIDEBAR_MIN_WINDOW) {
            scale.px(SIDEBAR_W)
        } else {
            0
        };
        let inspector_w =
            if panels.inspector && w - sidebar_w >= scale.px(INSPECTOR_W) + scale.px(CONTENT_MIN) {
                scale.px(INSPECTOR_W)
            } else {
                0
            };
        let column_x = sidebar_w;
        let column_w = (w - sidebar_w - inspector_w).max(1);
        // The header never takes the whole column: the content keeps a pixel.
        let header_h = if panels.header {
            scale.px(HEADER_H).min(body_h - 1).max(0)
        } else {
            0
        };

        Self {
            sidebar: Rect {
                x: 0,
                y: 0,
                w: sidebar_w,
                h: body_h,
            },
            header: Rect {
                x: column_x,
                y: 0,
                w: column_w,
                h: header_h,
            },
            content: Rect {
                x: column_x,
                y: header_h,
                w: column_w,
                h: (body_h - header_h).max(1),
            },
            inspector: Rect {
                x: column_x + column_w,
                y: 0,
                w: inspector_w,
                h: body_h,
            },
            status: Rect {
                x: 0,
                y: h - status_h,
                w,
                h: status_h,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::regions::*;

    const FRAMED: Panels = Panels {
        sidebar: true,
        header: true,
        inspector: false,
    };
    const ALL: Panels = Panels {
        sidebar: true,
        header: true,
        inspector: true,
    };
    const BARE: Panels = Panels {
        sidebar: false,
        header: false,
        inspector: false,
    };

    fn area(r: Rect) -> i64 {
        r.w as i64 * r.h as i64
    }

    fn overlap(a: Rect, b: Rect) -> bool {
        a.w > 0
            && a.h > 0
            && b.w > 0
            && b.h > 0
            && a.x < b.x + b.w
            && b.x < a.x + a.w
            && a.y < b.y + b.h
            && b.y < a.y + a.h
    }

    /// The shown regions cover the window exactly: no pixel left over, none
    /// claimed twice.
    fn assert_tiles(r: &Regions, w: i32, h: i32) {
        let all = [r.sidebar, r.header, r.content, r.inspector, r.status];
        for (i, a) in all.iter().enumerate() {
            assert!(a.x >= 0 && a.y >= 0, "{a:?} starts off the window");
            assert!(a.x + a.w <= w && a.y + a.h <= h, "{a:?} runs off {w}x{h}");
            for b in &all[i + 1..] {
                assert!(!overlap(*a, *b), "{a:?} overlaps {b:?} at {w}x{h}");
            }
        }
        let covered: i64 = all.iter().map(|&r| area(r)).sum();
        assert_eq!(covered, w as i64 * h as i64, "gaps at {w}x{h}: {r:?}");
    }

    #[test]
    fn the_regions_tile_the_window_at_every_width() {
        // Unity, fractional and integer scales: each rounds the logical sizes
        // differently, and none may leave a gap or a seam.
        for factor in [120, 150, 180, 240] {
            let scale = Scale::from_120(factor);
            for w in [200, 400, 559, 560, 561, 900, 1280, 3440] {
                for h in [150, 600, 1400] {
                    for panels in [FRAMED, ALL, BARE] {
                        assert_tiles(&Regions::new(scale, w, h, 30, panels), w, h);
                    }
                }
            }
        }
    }

    #[test]
    fn the_header_sits_over_the_content_column_and_the_sidebar_runs_full_height() {
        let r = Regions::new(Scale::ONE, 1000, 600, 30, FRAMED);
        assert_eq!(r.sidebar.w, SIDEBAR_W);
        assert_eq!(
            (r.sidebar.y, r.sidebar.h),
            (0, 570),
            "top to the status bar"
        );
        assert_eq!(
            r.header.x, SIDEBAR_W,
            "the header starts where the sidebar ends"
        );
        assert_eq!(r.header.w, 1000 - SIDEBAR_W);
        assert_eq!(r.header.h, HEADER_H);
        assert_eq!((r.content.x, r.content.w), (r.header.x, r.header.w));
        assert_eq!(r.content.y, HEADER_H);
        assert_eq!(r.content.y + r.content.h, 570);
        // The status bar alone spans the whole width.
        assert_eq!(
            r.status,
            Rect {
                x: 0,
                y: 570,
                w: 1000,
                h: 30
            }
        );
    }

    #[test]
    fn a_narrow_window_drops_the_sidebar_but_keeps_the_header() {
        let wide = Regions::new(Scale::ONE, SIDEBAR_MIN_WINDOW, 600, 30, FRAMED);
        assert_eq!(wide.sidebar.w, SIDEBAR_W);
        let narrow = Regions::new(Scale::ONE, SIDEBAR_MIN_WINDOW - 1, 600, 30, FRAMED);
        assert_eq!(narrow.sidebar.w, 0);
        assert_eq!(narrow.header.x, 0, "the header takes the whole width");
        assert_eq!(narrow.header.w, SIDEBAR_MIN_WINDOW - 1);
        assert_eq!(narrow.header.h, HEADER_H);
        assert_eq!(narrow.content.x, 0);
    }

    #[test]
    fn the_narrow_rule_is_measured_in_logical_pixels() {
        // At 2x a 1000-pixel buffer is a 500-point window: too narrow.
        let r = Regions::new(Scale::from_120(240), 1000, 1200, 60, FRAMED);
        assert_eq!(r.sidebar.w, 0);
        let r = Regions::new(Scale::from_120(240), 1200, 1200, 60, FRAMED);
        assert_eq!(r.sidebar.w, 2 * SIDEBAR_W);
        assert_eq!(r.header.h, 2 * HEADER_H);
    }

    #[test]
    fn with_no_panels_the_content_is_the_window_less_the_status_bar() {
        // The bare viewer's canvas: this must not move.
        let r = Regions::new(Scale::ONE, 900, 700, 30, BARE);
        assert_eq!(
            r.content,
            Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 670
            }
        );
        assert_eq!((r.sidebar.w, r.header.h), (0, 0));
    }

    #[test]
    fn a_window_shorter_than_its_chrome_still_has_a_content_pixel() {
        let r = Regions::new(Scale::ONE, 900, 50, 30, FRAMED);
        assert!(r.content.h >= 1);
        assert_tiles(&r, 900, 50);
        // Shorter than the status bar alone.
        let r = Regions::new(Scale::ONE, 900, 10, 30, FRAMED);
        assert!(r.content.h >= 1 && r.content.w >= 1);
        assert_tiles(&r, 900, 10);
    }

    #[test]
    fn the_inspector_runs_full_height_at_the_right_and_the_header_stops_short_of_it() {
        let r = Regions::new(Scale::ONE, 1200, 600, 30, ALL);
        assert_eq!(
            r.inspector,
            Rect {
                x: 1200 - INSPECTOR_W,
                y: 0,
                w: INSPECTOR_W,
                h: 570
            }
        );
        assert_eq!(r.header.x + r.header.w, r.inspector.x);
        assert_eq!(r.content.x + r.content.w, r.inspector.x);
        assert_eq!(r.content.w, 1200 - SIDEBAR_W - INSPECTOR_W);
    }

    #[test]
    fn a_narrowing_window_drops_the_inspector_before_the_sidebar() {
        // Just wide enough for both: the content keeps the column it has when
        // the sidebar is about to go.
        let both = SIDEBAR_W + INSPECTOR_W + CONTENT_MIN;
        let r = Regions::new(Scale::ONE, both, 600, 30, ALL);
        assert_eq!((r.sidebar.w, r.inspector.w), (SIDEBAR_W, INSPECTOR_W));
        assert_eq!(r.content.w, CONTENT_MIN);
        let r = Regions::new(Scale::ONE, both - 1, 600, 30, ALL);
        assert_eq!(
            (r.sidebar.w, r.inspector.w),
            (SIDEBAR_W, 0),
            "inspector first"
        );
        // Below the sidebar's own threshold neither is left, and an inspector
        // alone needs room for itself and the narrowest content.
        let r = Regions::new(Scale::ONE, SIDEBAR_MIN_WINDOW - 1, 600, 30, ALL);
        assert_eq!((r.sidebar.w, r.inspector.w), (0, 0));
        let alone = Panels {
            sidebar: false,
            ..ALL
        };
        let r = Regions::new(Scale::ONE, INSPECTOR_W + CONTENT_MIN, 600, 30, alone);
        assert_eq!(r.inspector.w, INSPECTOR_W);
        // An inspector not asked for is never shown, however wide the window.
        let r = Regions::new(Scale::ONE, 3440, 600, 30, FRAMED);
        assert_eq!(r.inspector.w, 0);
    }
}
