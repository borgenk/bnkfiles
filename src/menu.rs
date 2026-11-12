//! The command list: one overlay that is the context menu when the right
//! button opens it and the command palette when a key does.
//!
//! ```text
//!   ┌──────────────────────────────┐
//!   │ ren|                         │  typing narrows the list
//!   │ ▌Rename                  F2  │  the cursor row; Enter runs it
//!   │  Up to the parent  Backspace │
//!   └──────────────────────────────┘
//! ```
//!
//! Pure: the commands come from [`crate::keymap`]'s table, each with the key
//! that runs it, and this holds which of them the text lets by, where the
//! cursor is, and where the panel and its rows land. The overlay is modal
//! while it is up, like the prompt.

use crate::filter::Filter;
use crate::keymap::Command;
use crate::platform::geom::{Rect, Scale};
use crate::prompt::TextField;

/// Where the list opens: in the middle of the window as a palette, or at the
/// pointer as a context menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    Centre,
    At(i32, i32),
}

/// The panel's width, the most rows it shows at once, and its inner padding.
/// Logical pixels.
const WIDTH: i32 = 360;
const MAX_ROWS: usize = 12;
const PAD: i32 = 8;

pub struct Menu {
    pub anchor: Anchor,
    items: Vec<Command>,
    filter: Filter,
    cursor: usize,
}

impl Menu {
    /// A list of `items`, in the order they are offered.
    pub fn new(items: Vec<Command>, anchor: Anchor) -> Self {
        Self {
            anchor,
            items,
            filter: Filter::new(),
            cursor: 0,
        }
    }

    pub fn field(&self) -> &TextField {
        self.filter.field()
    }

    /// The commands the typed text lets by, matched against their titles.
    pub fn shown(&self) -> Vec<Command> {
        self.items
            .iter()
            .copied()
            .filter(|c| self.filter.matches(c.title()))
            .collect()
    }

    /// The row the cursor is on, among [`Self::shown`].
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Edit the text. What it lets by changes, so the cursor goes back to
    /// the first of them.
    pub fn edit(&mut self, edit: impl FnOnce(&mut TextField)) {
        if self.filter.edit(edit) {
            self.cursor = 0;
        }
    }

    /// Move the cursor by `delta`, stopping at the ends.
    pub fn move_cursor(&mut self, delta: i32) {
        let last = self.shown().len().saturating_sub(1) as i32;
        self.cursor = (self.cursor as i32 + delta).clamp(0, last.max(0)) as usize;
    }

    /// Put the cursor on row `row` of [`Self::shown`].
    pub fn point(&mut self, row: usize) {
        if row < self.shown().len() {
            self.cursor = row;
        }
    }

    /// The command the cursor is on, if any command is shown.
    pub fn chosen(&self) -> Option<Command> {
        self.shown().get(self.cursor).copied()
    }
}

/// Where the panel, its text field and its rows land.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuLayout {
    pub panel: Rect,
    pub field: Rect,
    /// The rows on show, each with its index among the shown commands.
    pub rows: Vec<(usize, Rect)>,
}

impl MenuLayout {
    /// Lay out `count` shown commands with the cursor on `cursor`, rows
    /// `row_h` tall, inside `window`. A palette sits a third of the way down,
    /// centred; a context menu opens with its corner at the point, moved in
    /// so it never runs off the window. Past [`MAX_ROWS`] the rows scroll to
    /// keep the cursor's on show.
    pub fn new(
        scale: Scale,
        window: Rect,
        anchor: Anchor,
        count: usize,
        cursor: usize,
        row_h: i32,
    ) -> Self {
        let pad = scale.px(PAD);
        // As many rows as the window has room for under the field, so the
        // cursor's row is always one of those drawn.
        let room = ((window.h - 2 * pad) / row_h.max(1) - 1).max(1) as usize;
        let visible = count.clamp(1, MAX_ROWS).min(room);
        let w = scale.px(WIDTH).min(window.w).max(1);
        let h = (2 * pad + row_h * (1 + visible as i32))
            .min(window.h)
            .max(1);
        let (x, y) = match anchor {
            Anchor::Centre => (
                window.x + (window.w - w) / 2,
                window.y + window.h / 3 - h / 3,
            ),
            Anchor::At(x, y) => (x, y),
        };
        let panel = Rect {
            x: x.clamp(window.x, (window.x + window.w - w).max(window.x)),
            y: y.clamp(window.y, (window.y + window.h - h).max(window.y)),
            w,
            h,
        };
        let field = Rect {
            x: panel.x + pad,
            y: panel.y + pad,
            w: (w - 2 * pad).max(0),
            h: row_h,
        };
        let first = (cursor + 1).saturating_sub(visible);
        let rows = (first..count.min(first + visible))
            .enumerate()
            .map(|(i, index)| {
                (
                    index,
                    Rect {
                        x: field.x,
                        y: field.y + row_h * (1 + i as i32),
                        w: field.w,
                        h: row_h,
                    },
                )
            })
            .filter(|(_, r)| r.y + r.h <= panel.y + panel.h)
            .collect();
        Self { panel, field, rows }
    }

    /// The shown command's index at window point `(px, py)`, if the point
    /// is on a row.
    pub fn row_at(&self, px: f32, py: f32) -> Option<usize> {
        self.rows
            .iter()
            .find(|(_, r)| crate::explorer::contains(*r, px, py))
            .map(|(i, _)| *i)
    }
}

#[cfg(test)]
mod tests {
    use crate::menu::*;

    fn items() -> Vec<Command> {
        vec![
            Command::Open,
            Command::Rename,
            Command::Trash,
            Command::CopyPath,
            Command::Quit,
        ]
    }

    #[test]
    fn typing_narrows_the_list_and_puts_the_cursor_on_the_first_left() {
        let mut m = Menu::new(items(), Anchor::Centre);
        m.move_cursor(3);
        assert_eq!(m.chosen(), Some(Command::CopyPath));
        for c in "ren".chars() {
            m.edit(|f| f.insert(c));
        }
        assert_eq!(m.shown(), [Command::Rename]);
        assert_eq!((m.cursor(), m.chosen()), (0, Some(Command::Rename)));
        m.edit(|f| f.insert('z'));
        assert_eq!(m.chosen(), None, "nothing matches, nothing to run");
        m.move_cursor(1);
        assert_eq!(m.cursor(), 0);
    }

    #[test]
    fn the_cursor_stops_at_the_ends() {
        let mut m = Menu::new(items(), Anchor::Centre);
        m.move_cursor(-1);
        assert_eq!(m.cursor(), 0);
        m.move_cursor(99);
        assert_eq!(m.chosen(), Some(Command::Quit));
        m.point(1);
        assert_eq!(m.chosen(), Some(Command::Rename));
        m.point(99);
        assert_eq!(m.chosen(), Some(Command::Rename), "no row there");
    }

    fn window() -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: 900,
            h: 600,
        }
    }

    #[test]
    fn a_context_menu_opens_at_the_point_and_stays_on_the_window() {
        let l = MenuLayout::new(Scale::ONE, window(), Anchor::At(100, 50), 5, 0, 28);
        assert_eq!((l.panel.x, l.panel.y), (100, 50));
        // Near the bottom right corner: moved in, not run off.
        let l = MenuLayout::new(Scale::ONE, window(), Anchor::At(890, 590), 5, 0, 28);
        assert!(l.panel.x + l.panel.w <= 900 && l.panel.y + l.panel.h <= 600);
        assert!(l.panel.x >= 0 && l.panel.y >= 0);
        // A window smaller than the panel: the panel shrinks to it.
        let tiny = Rect {
            x: 0,
            y: 0,
            w: 200,
            h: 100,
        };
        let l = MenuLayout::new(Scale::ONE, tiny, Anchor::Centre, 20, 0, 28);
        assert!(l.panel.w <= 200 && l.panel.h <= 100);
        for (_, r) in &l.rows {
            assert!(r.y + r.h <= l.panel.y + l.panel.h, "a row past the panel");
        }
        // And the cursor's row is one of those drawn, however far down.
        let l = MenuLayout::new(Scale::ONE, tiny, Anchor::Centre, 20, 15, 28);
        assert_eq!(l.rows.last().map(|(i, _)| *i), Some(15));
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_cursor_on_show() {
        let l = MenuLayout::new(Scale::ONE, window(), Anchor::Centre, 30, 0, 28);
        assert_eq!(l.rows.len(), MAX_ROWS);
        assert_eq!(l.rows[0].0, 0);
        let l = MenuLayout::new(Scale::ONE, window(), Anchor::Centre, 30, 20, 28);
        assert_eq!(l.rows.last().map(|(i, _)| *i), Some(20));
        // Rows stack under the field without gaps or overlaps.
        for pair in l.rows.windows(2) {
            assert_eq!(pair[0].1.y + 28, pair[1].1.y);
        }
        assert_eq!(l.rows[0].1.y, l.field.y + 28);
    }

    #[test]
    fn a_click_lands_on_the_row_it_is_drawn_on() {
        let l = MenuLayout::new(Scale::ONE, window(), Anchor::At(100, 50), 5, 0, 28);
        for (i, r) in &l.rows {
            let (cx, cy) = ((r.x + r.w / 2) as f32, (r.y + r.h / 2) as f32);
            assert_eq!(l.row_at(cx, cy), Some(*i));
        }
        let f = l.field;
        assert_eq!(
            l.row_at((f.x + 5) as f32, (f.y + 5) as f32),
            None,
            "the field"
        );
        assert_eq!(l.row_at(5.0, 5.0), None, "outside the panel");
    }
}
