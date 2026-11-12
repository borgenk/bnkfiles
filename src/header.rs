//! The header's controls: which buttons it holds in each view, where each one
//! sits, and which one a point lands on.
//!
//! ```text
//!   explorer  [←][→][↑]  / › home › me › pics ........ [▦][◨] [⌕ filter  ]
//!   viewer    [←]  2 / 6  name.jpg ............... 40%  [1:1][Fit][‹][›]
//!             └ left group ┘└──────── middle ───────────┘└ right group ┘
//! ```
//!
//! The explorer's filter field sits at the right end, past its buttons. It is
//! the first thing a narrowing header gives up: it shrinks, then goes, before
//! any button does, and it never takes the middle below [`MIDDLE_MIN`].
//!
//! Pure geometry, like [`crate::explorer`]: the painter draws the buttons
//! where this says and the click handler asks this which one was hit, so the
//! two cannot disagree. Device pixels; the constants are logical.

use crate::platform::geom::{Rect, Scale};

/// A button's side, the gap between buttons, and the inset from the header's
/// edges and between a group and the middle. Logical pixels.
pub const BUTTON: i32 = 32;
const GAP: i32 = 4;
const PAD: i32 = 8;
/// The filter field's width, the narrowest it is still drawn at, and the room
/// it always leaves the breadcrumb. Logical pixels.
const FIELD_W: i32 = 200;
const FIELD_MIN: i32 = 96;
const MIDDLE_MIN: i32 = 96;

/// One header button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    /// Explorer: the previous and next places in the history, and the parent.
    Back,
    Forward,
    Up,
    /// Explorer: switch between the list and the grid, and show or hide the
    /// inspector.
    Layout,
    Inspector,
    /// Viewer: back to the listing.
    Close,
    /// Viewer: actual size, fit, and the previous and next picture.
    Actual,
    Fit,
    Previous,
    Next,
}

/// Which header is showing: the explorer's or the viewer's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderKind {
    Explorer,
    Viewer,
}

impl HeaderKind {
    /// Whether this header has the filter field.
    fn has_field(self) -> bool {
        self == HeaderKind::Explorer
    }

    /// The buttons at the left end and the right end, each left to right.
    fn groups(self) -> (&'static [Control], &'static [Control]) {
        match self {
            HeaderKind::Explorer => (
                &[Control::Back, Control::Forward, Control::Up],
                &[Control::Layout, Control::Inspector],
            ),
            HeaderKind::Viewer => (
                &[Control::Close],
                &[
                    Control::Actual,
                    Control::Fit,
                    Control::Previous,
                    Control::Next,
                ],
            ),
        }
    }
}

/// Where the header's buttons and its middle land.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderLayout {
    pub controls: Vec<(Control, Rect)>,
    /// What is left between the two groups: the breadcrumb in the explorer,
    /// the position, name and zoom in the viewer. May be zero wide.
    pub middle: Rect,
    /// The filter field, when this header has one and there is room for it.
    pub field: Option<Rect>,
}

impl HeaderLayout {
    /// Lay `kind`'s header out in `header`. A header with no height has no
    /// controls at all, so nothing in it can be hit.
    ///
    /// When the header is too narrow for both groups the right group is
    /// dropped first, then the left one, so the middle never goes negative and
    /// no two buttons overlap.
    pub fn new(scale: Scale, header: Rect, kind: HeaderKind) -> Self {
        if header.h <= 0 || header.w <= 0 {
            return Self {
                controls: Vec::new(),
                middle: Rect { w: 0, ..header },
                field: None,
            };
        }
        let side = scale.px(BUTTON).min(header.h);
        let gap = scale.px(GAP);
        let pad = scale.px(PAD);
        let y = header.y + (header.h - side) / 2;
        let width = |n: usize| {
            if n == 0 {
                0
            } else {
                n as i32 * side + (n as i32 - 1) * gap
            }
        };

        let (mut left, mut right) = kind.groups();
        let fits = |l: &[Control], r: &[Control]| {
            let mut used = pad * 2;
            if !l.is_empty() {
                used += width(l.len()) + pad;
            }
            if !r.is_empty() {
                used += width(r.len()) + pad;
            }
            used <= header.w
        };
        if !fits(left, right) {
            right = &[];
        }
        if !fits(left, right) {
            left = &[];
        }
        // The field takes what the buttons leave, less the middle's share.
        let buttons = |n: usize| if n == 0 { 0 } else { width(n) + pad };
        let spare = header.w - 2 * pad - buttons(left.len()) - buttons(right.len());
        let field_w = (spare - scale.px(MIDDLE_MIN) - pad).min(scale.px(FIELD_W));
        let field = (kind.has_field() && field_w >= scale.px(FIELD_MIN)).then(|| Rect {
            x: header.x + header.w - pad - field_w,
            y,
            w: field_w,
            h: side,
        });
        // Everything right of the middle ends here: the field's left edge, or
        // the header's own end.
        let end = field.map_or(header.x + header.w, |f| f.x);

        let mut controls = Vec::with_capacity(left.len() + right.len());
        let mut x = header.x + pad;
        for &c in left {
            controls.push((c, square(x, y, side)));
            x += side + gap;
        }
        let middle_x = if left.is_empty() {
            header.x + pad
        } else {
            header.x + pad + width(left.len()) + pad
        };
        let right_x = end - pad - width(right.len());
        let mut x = right_x;
        for &c in right {
            controls.push((c, square(x, y, side)));
            x += side + gap;
        }
        let middle_end = if right.is_empty() {
            end - pad
        } else {
            right_x - pad
        };
        Self {
            controls,
            middle: Rect {
                x: middle_x,
                y: header.y,
                w: (middle_end - middle_x).max(0),
                h: header.h,
            },
            field,
        }
    }

    /// The control at window point `(px, py)`, if the point is on one.
    pub fn control_at(&self, px: f32, py: f32) -> Option<Control> {
        self.controls
            .iter()
            .find(|(_, r)| crate::explorer::contains(*r, px, py))
            .map(|(c, _)| *c)
    }

    /// Whether window point `(px, py)` is on the filter field.
    pub fn on_field(&self, px: f32, py: f32) -> bool {
        self.field
            .is_some_and(|f| crate::explorer::contains(f, px, py))
    }
}

fn square(x: i32, y: i32, side: i32) -> Rect {
    Rect {
        x,
        y,
        w: side,
        h: side,
    }
}

#[cfg(test)]
mod tests {
    use crate::header::*;

    fn header(x: i32, w: i32) -> Rect {
        Rect { x, y: 0, w, h: 44 }
    }

    fn overlap(a: Rect, b: Rect) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    fn within(inner: Rect, outer: Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.x + inner.w <= outer.x + outer.w
            && inner.y + inner.h <= outer.y + outer.h
    }

    #[test]
    fn each_view_gets_its_own_buttons_in_order() {
        let l = HeaderLayout::new(Scale::ONE, header(190, 710), HeaderKind::Explorer);
        let kinds: Vec<Control> = l.controls.iter().map(|(c, _)| *c).collect();
        assert_eq!(
            kinds,
            [
                Control::Back,
                Control::Forward,
                Control::Up,
                Control::Layout,
                Control::Inspector
            ]
        );
        let l = HeaderLayout::new(Scale::ONE, header(190, 710), HeaderKind::Viewer);
        let kinds: Vec<Control> = l.controls.iter().map(|(c, _)| *c).collect();
        assert_eq!(
            kinds,
            [
                Control::Close,
                Control::Actual,
                Control::Fit,
                Control::Previous,
                Control::Next
            ]
        );
    }

    #[test]
    fn buttons_and_the_middle_never_overlap_at_any_width() {
        for kind in [HeaderKind::Explorer, HeaderKind::Viewer] {
            for w in [1, 20, 60, 120, 160, 200, 300, 710, 2000] {
                for scale in [Scale::ONE, Scale::from_120(180), Scale::from_120(240)] {
                    let h = header(190, w);
                    let l = HeaderLayout::new(scale, h, kind);
                    let rects: Vec<Rect> = l.controls.iter().map(|(_, r)| *r).collect();
                    let field = l.field.into_iter();
                    let rects: Vec<Rect> = rects.into_iter().chain(field).collect();
                    for (i, a) in rects.iter().enumerate() {
                        assert!(within(*a, h), "{a:?} leaves the header at {w}");
                        assert!(!overlap(*a, l.middle), "{a:?} over the middle at {w}");
                        for b in &rects[i + 1..] {
                            assert!(!overlap(*a, *b), "{a:?} overlaps {b:?} at {w}");
                        }
                    }
                    assert!(l.middle.w >= 0);
                }
            }
        }
    }

    #[test]
    fn a_click_lands_on_the_button_it_is_drawn_on() {
        let l = HeaderLayout::new(Scale::ONE, header(190, 710), HeaderKind::Explorer);
        for (c, r) in &l.controls {
            let (cx, cy) = ((r.x + r.w / 2) as f32, (r.y + r.h / 2) as f32);
            assert_eq!(l.control_at(cx, cy), Some(*c));
            // The edges: left and top inclusive, right and bottom exclusive.
            assert_eq!(l.control_at(r.x as f32, r.y as f32), Some(*c));
            assert_ne!(l.control_at((r.x + r.w) as f32, cy), Some(*c));
        }
        // The gap between two buttons, and the middle, hit nothing.
        let (_, back) = l.controls[0];
        assert_eq!(l.control_at((back.x + back.w) as f32 + 1.0, 22.0), None);
        assert_eq!(
            l.control_at((l.middle.x + l.middle.w / 2) as f32, 22.0),
            None
        );
        // Neither does the sidebar to the left of the header.
        assert_eq!(l.control_at(10.0, 22.0), None);
    }

    #[test]
    fn the_buttons_sit_at_the_ends_and_the_middle_between() {
        let h = header(190, 710);
        let l = HeaderLayout::new(Scale::ONE, h, HeaderKind::Explorer);
        let (_, back) = l.controls[0];
        let (_, last) = l.controls[4];
        let field = l.field.expect("room for the field");
        assert_eq!(back.x, 190 + PAD);
        assert_eq!(field.x + field.w, 190 + 710 - PAD, "the field ends the row");
        assert_eq!((field.w, field.y, field.h), (FIELD_W, back.y, BUTTON));
        assert_eq!(last.x + last.w, field.x - PAD);
        assert_eq!((back.y, back.h), ((44 - BUTTON) / 2, BUTTON));
        assert!(l.middle.x > back.x + 3 * BUTTON);
        assert!(l.middle.x + l.middle.w < l.controls[3].1.x);
    }

    #[test]
    fn a_narrow_header_drops_the_right_group_first() {
        // Room for the explorer's three left buttons but not the fourth.
        let w = 2 * PAD + 3 * BUTTON + 2 * GAP + PAD + 10;
        let l = HeaderLayout::new(Scale::ONE, header(0, w), HeaderKind::Explorer);
        let kinds: Vec<Control> = l.controls.iter().map(|(c, _)| *c).collect();
        assert_eq!(kinds, [Control::Back, Control::Forward, Control::Up]);
        // Too narrow for anything: no buttons, and the middle is what is left.
        let l = HeaderLayout::new(Scale::ONE, header(0, 30), HeaderKind::Explorer);
        assert!(l.controls.is_empty());
    }

    #[test]
    fn the_field_shrinks_then_goes_before_any_button_and_leaves_the_middle_room() {
        let s = Scale::ONE;
        let at = |w: i32| HeaderLayout::new(s, header(0, w), HeaderKind::Explorer);
        let mut last_w = FIELD_W;
        let mut gone_at = None;
        for w in (100..=800).rev() {
            let l = at(w);
            match l.field {
                Some(f) => {
                    assert!(f.w <= last_w, "the field only shrinks as the header does");
                    assert!(f.w >= FIELD_MIN);
                    assert!(l.middle.w >= MIDDLE_MIN, "middle squeezed at {w}");
                    assert_eq!(l.controls.len(), 5, "a button went before the field at {w}");
                    last_w = f.w;
                }
                None => {
                    gone_at.get_or_insert(w);
                }
            }
            assert!(
                !l.on_field(5.0, 22.0),
                "the left end is a button or the middle"
            );
        }
        let gone_at = gone_at.expect("a narrow enough header has no field");
        assert_eq!(
            at(gone_at).controls.len(),
            5,
            "every button outlives the field"
        );
        // The viewer never has one.
        let l = HeaderLayout::new(s, header(0, 2000), HeaderKind::Viewer);
        assert_eq!(l.field, None);
    }

    #[test]
    fn a_click_on_the_field_is_on_the_field_and_on_no_button() {
        let l = HeaderLayout::new(Scale::ONE, header(190, 710), HeaderKind::Explorer);
        let f = l.field.expect("room for the field");
        let (cx, cy) = ((f.x + f.w / 2) as f32, (f.y + f.h / 2) as f32);
        assert!(l.on_field(cx, cy));
        assert_eq!(l.control_at(cx, cy), None);
        assert!(
            !l.on_field((f.x + f.w) as f32, cy),
            "the right edge is outside"
        );
    }

    #[test]
    fn a_header_that_is_not_shown_has_nothing_to_hit() {
        let hidden = Rect {
            x: 0,
            y: 0,
            w: 900,
            h: 0,
        };
        let l = HeaderLayout::new(Scale::ONE, hidden, HeaderKind::Viewer);
        assert!(l.controls.is_empty());
        assert_eq!(l.control_at(20.0, 0.0), None);
    }
}
