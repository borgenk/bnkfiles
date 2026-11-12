//! Which entries are selected, as a value.
//!
//! A file manager's selection is a small state machine that everyone gets subtly
//! wrong in the same places: what Shift measures from after a Ctrl-click, whether
//! extending moves the anchor, what happens when the thing you had selected is
//! deleted underneath you. Keeping it here — no I/O, no Wayland types, no view —
//! means those answers are pinned by tests rather than rediscovered from whichever
//! call site last touched them.
//!
//! Three pieces of state, which is the whole model:
//!
//! - **lead** — the row with the keyboard's attention. What the arrows move, what
//!   Enter opens, what the view scrolls into sight.
//! - **anchor** — where a Shift range measures from. It moves on a plain click or
//!   a Ctrl-click and stays put while extending, which is what makes Shift+Down
//!   grow a range instead of dragging a two-row window down the list.
//! - **selected** — the rows themselves, ordered, so a caller acting on them (a
//!   copy, a delete) walks the directory in the order it is displayed.
//!
//! ```text
//!   click(3)          anchor=3 lead=3  {3}
//!   shift_extend(6)   anchor=3 lead=6  {3,4,5,6}     anchor stayed
//!   ctrl_toggle(1)    anchor=1 lead=1  {1,3,4,5,6}   anchor moved
//!   shift_extend(2)   anchor=1 lead=2  {1,2}         measured from the new anchor
//! ```

use std::collections::BTreeSet;

#[derive(Default)]
pub struct Selection {
    /// How many rows exist to select from. Everything below is kept inside it.
    len: usize,
    anchor: usize,
    lead: usize,
    selected: BTreeSet<usize>,
}

impl Selection {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tell the selection how many rows there are now.
    ///
    /// Called whenever the listing is re-projected, which a streaming scan does on
    /// every batch. Rows that no longer exist drop out rather than lingering as
    /// indices into a list that has moved on; the cursors clamp into range. This is
    /// the one place a shrinking directory is dealt with, so no caller has to.
    pub fn set_len(&mut self, len: usize) {
        self.len = len;
        if len == 0 {
            self.selected.clear();
            self.anchor = 0;
            self.lead = 0;
            return;
        }
        self.selected.retain(|&i| i < len);
        self.anchor = self.anchor.min(len - 1);
        self.lead = self.lead.min(len - 1);
    }

    /// Carry the selection across a change of rows: a rescan, or dot-files shown
    /// or hidden. `moved` says where each old row is now, or `None` if it is gone.
    ///
    /// Rows are positions, and positions shift when the list under them changes:
    /// left alone, the selection would end up on whatever slid into its place,
    /// and the next Delete would take that instead.
    ///
    /// A cursor whose row is gone stays at its position, which is now the entry
    /// that followed it. If it was selected, that entry is selected in its place,
    /// so deleting down a list one key at a time keeps a visible cursor.
    pub fn remap(&mut self, len: usize, moved: impl Fn(usize) -> Option<usize>) {
        let lead_was_selected = self.selected.contains(&self.lead);
        let moved_in = |row: usize| moved(row).filter(|&now| now < len);
        self.selected = self
            .selected
            .iter()
            .filter_map(|&row| moved_in(row))
            .collect();
        let (anchor, lead) = (moved_in(self.anchor), moved_in(self.lead));
        self.set_len(len);
        if len == 0 {
            return;
        }
        match lead {
            Some(lead) => self.lead = lead,
            None if lead_was_selected && self.selected.is_empty() => self.click(self.lead),
            None => {}
        }
        // An anchor that is gone restarts the next Shift range from the cursor.
        self.anchor = anchor.unwrap_or(self.lead);
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// The row with the keyboard's attention, or `None` when there is nothing to
    /// have it.
    pub fn lead(&self) -> Option<usize> {
        (self.len > 0).then_some(self.lead)
    }

    pub fn is_selected(&self, row: usize) -> bool {
        self.selected.contains(&row)
    }

    pub fn count(&self) -> usize {
        self.selected.len()
    }

    /// The selected rows, in display order.
    ///
    /// Ordered because the operations that consume it act on a directory: copying
    /// or deleting a run of files should walk them the way they are shown, not the
    /// way a hash set happens to hold them.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.selected.iter().copied()
    }

    /// Every row the selection has a stake in: the selected ones and both cursors.
    pub fn marked(&self) -> impl Iterator<Item = usize> + '_ {
        let cursors = (self.len > 0).then_some([self.anchor, self.lead]);
        self.iter().chain(cursors.into_iter().flatten())
    }

    /// A plain click or an unextended keyboard move: `row` alone is selected, and
    /// the next Shift range will measure from it.
    pub fn click(&mut self, row: usize) {
        let Some(row) = self.clamp(row) else { return };
        self.selected.clear();
        self.selected.insert(row);
        self.anchor = row;
        self.lead = row;
    }

    /// Put the cursor on `row` and leave the selection as it is: a right-click
    /// on an entry already in the selection, which the menu then acts on.
    pub fn point(&mut self, row: usize) {
        if let Some(row) = self.clamp(row) {
            self.lead = row;
            self.anchor = row;
        }
    }

    /// Shift-click, or an extending keyboard move: select the whole run between the
    /// anchor and `row`. The anchor does not move, so extending again from the same
    /// anchor re-measures rather than accumulating.
    pub fn shift_extend(&mut self, row: usize) {
        let Some(row) = self.clamp(row) else { return };
        let (lo, hi) = (self.anchor.min(row), self.anchor.max(row));
        self.selected.clear();
        self.selected.extend(lo..=hi);
        self.lead = row;
    }

    /// Ctrl-click: add `row` to the selection, or take it out if it was in it.
    /// Either way it becomes the anchor, so a Shift range afterwards measures from
    /// where the user last pointed rather than from wherever they were before.
    pub fn ctrl_toggle(&mut self, row: usize) {
        let Some(row) = self.clamp(row) else { return };
        if !self.selected.remove(&row) {
            self.selected.insert(row);
        }
        self.anchor = row;
        self.lead = row;
    }

    /// Move the lead by `delta` rows, clamped to the ends of the list. With
    /// `extending`, the run from the anchor grows or shrinks to follow; without it,
    /// the lead is simply the new single selection.
    ///
    /// Clamped, not wrapped: an arrow held at the bottom of a directory should stop
    /// there, not reappear at the top. (The viewer wraps, because walking a folder
    /// of photographs in a ring is useful; walking a selection in one is not.)
    pub fn move_lead(&mut self, delta: i32, extending: bool) {
        if self.len == 0 {
            return;
        }
        let last = (self.len - 1) as i64;
        let next = (self.lead as i64)
            .saturating_add(delta as i64)
            .clamp(0, last) as usize;
        if extending {
            self.shift_extend(next);
        } else {
            self.click(next);
        }
    }

    pub fn select_all(&mut self) {
        if self.len == 0 {
            return;
        }
        self.selected.clear();
        self.selected.extend(0..self.len);
        self.anchor = 0;
        self.lead = self.len - 1;
    }

    /// Deselect everything. The lead stays where it is: clearing a selection is not
    /// a reason to lose your place in the directory.
    pub fn clear(&mut self) {
        self.selected.clear();
    }

    fn clamp(&self, row: usize) -> Option<usize> {
        (self.len > 0).then(|| row.min(self.len - 1))
    }
}

#[cfg(test)]
mod tests {
    use crate::select::*;

    /// A selection over `len` rows, for readable scenarios below.
    fn sel(len: usize) -> Selection {
        let mut s = Selection::new();
        s.set_len(len);
        s
    }

    /// The selected rows, as a vector, in display order.
    fn rows(s: &Selection) -> Vec<usize> {
        s.iter().collect()
    }

    #[test]
    fn a_click_selects_one_row_and_anchors_there() {
        let mut s = sel(10);
        s.click(3);
        assert_eq!(rows(&s), vec![3]);
        assert_eq!(s.lead(), Some(3));

        // A second click replaces the first rather than adding to it.
        s.click(7);
        assert_eq!(rows(&s), vec![7]);
        assert_eq!(s.lead(), Some(7));
    }

    #[test]
    fn shift_extends_from_the_anchor_and_leaves_it_alone() {
        let mut s = sel(10);
        s.click(3);
        s.shift_extend(6);
        assert_eq!(rows(&s), vec![3, 4, 5, 6]);
        assert_eq!(s.lead(), Some(6));

        // Extending again re-measures from the same anchor. This is the difference
        // that matters: a moving anchor would drag a fixed-size window instead.
        s.shift_extend(4);
        assert_eq!(rows(&s), vec![3, 4]);
        // And backwards past the anchor selects the other side of it.
        s.shift_extend(1);
        assert_eq!(rows(&s), vec![1, 2, 3]);
        assert_eq!(s.lead(), Some(1));
    }

    #[test]
    fn ctrl_toggles_one_row_and_moves_the_anchor_to_it() {
        let mut s = sel(10);
        s.click(3);
        s.shift_extend(6);
        s.ctrl_toggle(1);
        assert_eq!(
            rows(&s),
            vec![1, 3, 4, 5, 6],
            "the run survives, 1 joins it"
        );

        // The anchor followed the Ctrl-click, so the next Shift measures from 1.
        s.shift_extend(2);
        assert_eq!(rows(&s), vec![1, 2]);

        // Toggling a selected row takes it back out.
        s.ctrl_toggle(2);
        assert_eq!(rows(&s), vec![1]);
        s.ctrl_toggle(1);
        assert_eq!(rows(&s), Vec::<usize>::new());
    }

    #[test]
    fn arrows_move_the_lead_and_extending_grows_the_run() {
        let mut s = sel(5);
        s.click(0);
        s.move_lead(1, false);
        assert_eq!(rows(&s), vec![1], "a plain move is a new single selection");

        s.move_lead(1, true);
        assert_eq!(rows(&s), vec![1, 2], "extending grows from the anchor at 1");
        s.move_lead(1, true);
        assert_eq!(rows(&s), vec![1, 2, 3]);
        // Shrinking back through the anchor flips the run to the other side.
        s.move_lead(-3, true);
        assert_eq!(rows(&s), vec![0, 1]);
    }

    #[test]
    fn the_lead_stops_at_the_ends_rather_than_wrapping() {
        let mut s = sel(3);
        s.click(0);
        s.move_lead(-1, false);
        assert_eq!(s.lead(), Some(0), "up from the top stays at the top");
        s.move_lead(99, false);
        assert_eq!(
            s.lead(),
            Some(2),
            "a page past the end lands on the last row"
        );
        s.move_lead(1, false);
        assert_eq!(s.lead(), Some(2));
        // A delta big enough to overflow the arithmetic still just clamps.
        s.move_lead(i32::MIN, false);
        assert_eq!(s.lead(), Some(0));
    }

    #[test]
    fn select_all_takes_everything_and_clear_keeps_your_place() {
        let mut s = sel(4);
        s.click(2);
        s.select_all();
        assert_eq!(rows(&s), vec![0, 1, 2, 3]);

        s.clear();
        assert_eq!(rows(&s), Vec::<usize>::new());
        assert_eq!(
            s.lead(),
            Some(3),
            "clearing a selection does not move the cursor"
        );
    }

    #[test]
    fn a_shrinking_listing_drops_the_rows_that_went_away() {
        // A directory being rescanned, or files deleted from under the selection.
        let mut s = sel(10);
        s.click(2);
        s.shift_extend(8);
        assert_eq!(rows(&s), vec![2, 3, 4, 5, 6, 7, 8]);

        s.set_len(5);
        assert_eq!(
            rows(&s),
            vec![2, 3, 4],
            "only rows that still exist survive"
        );
        assert_eq!(s.lead(), Some(4), "the lead clamps into the shorter list");

        // Emptied entirely: no selection, no lead, and no panic from either.
        s.set_len(0);
        assert_eq!(rows(&s), Vec::<usize>::new());
        assert_eq!(s.lead(), None);
        assert_eq!(s.count(), 0);
    }

    #[test]
    fn an_empty_selection_ignores_every_operation() {
        // The window before a scan has landed: every entry point is reachable and
        // none of them may panic or invent a selection out of nothing.
        let mut s = sel(0);
        s.click(4);
        s.shift_extend(2);
        s.ctrl_toggle(1);
        s.move_lead(3, true);
        s.select_all();
        assert_eq!(rows(&s), Vec::<usize>::new());
        assert_eq!(s.lead(), None);
    }

    #[test]
    fn a_row_past_the_end_clamps_instead_of_being_ignored() {
        // Hit tests and restored positions can name a row that is no longer there;
        // landing on the last row beats landing on nothing.
        let mut s = sel(3);
        s.click(99);
        assert_eq!(rows(&s), vec![2]);
        assert_eq!(s.lead(), Some(2));
        s.shift_extend(99);
        assert_eq!(rows(&s), vec![2]);
    }

    /// A selection of rows `rows` with the cursor on the last of them.
    fn selecting(len: usize, rows: &[usize]) -> Selection {
        let mut sel = Selection::new();
        sel.set_len(len);
        for &row in rows {
            sel.ctrl_toggle(row);
        }
        sel
    }

    #[test]
    fn a_refresh_keeps_the_selection_on_the_same_entries() {
        // Rows 2 and 5 selected; a file appears above them, so everything from
        // row 1 down shifts by one.
        let mut sel = selecting(8, &[2, 5]);
        sel.remap(9, |row| Some(if row >= 1 { row + 1 } else { row }));
        assert_eq!(sel.iter().collect::<Vec<_>>(), [3, 6]);
        assert_eq!(sel.lead(), Some(6));
    }

    #[test]
    fn a_deleted_cursor_moves_to_the_entry_that_followed_it_not_the_top() {
        // The cursor's own row is deleted; later rows close up.
        let gone = |at: usize| {
            move |row: usize| match row.cmp(&at) {
                std::cmp::Ordering::Less => Some(row),
                std::cmp::Ordering::Equal => None,
                std::cmp::Ordering::Greater => Some(row - 1),
            }
        };
        let mut sel = selecting(8, &[4]);
        sel.remap(7, gone(4));
        assert_eq!(sel.lead(), Some(4), "the old neighbour, now in its place");
        assert_eq!(
            sel.iter().collect::<Vec<_>>(),
            [4],
            "and it shows as the cursor"
        );

        // The last row deleted: the cursor steps back onto the new last row.
        let mut sel = selecting(8, &[7]);
        sel.remap(7, gone(7));
        assert_eq!(sel.lead(), Some(6));
    }

    #[test]
    fn rows_that_vanish_leave_the_selection_rather_than_pass_it_on() {
        // Dot-files hidden: rows 0 and 1 go, the rest move up two. The selection
        // was the two hidden rows, and must not land on what replaced them.
        let mut sel = selecting(6, &[0, 1]);
        sel.clear();
        sel.shift_extend(0);
        assert_eq!(sel.count(), 2);
        sel.remap(4, |row| row.checked_sub(2));
        assert!(!sel.is_selected(1), "row 1 is a different file now");
        assert_eq!(sel.count(), 1, "only the cursor's stand-in is selected");
    }

    #[test]
    fn a_selection_survives_an_unselected_cursor_vanishing() {
        let mut sel = selecting(6, &[1, 2]);
        sel.clear();
        sel.shift_extend(1);
        // Cursor is row 1 (selected along with 2); delete row 5, which is neither.
        sel.remap(5, |row| (row < 5).then_some(row));
        assert_eq!(sel.iter().collect::<Vec<_>>(), [1, 2]);

        // An emptied directory resets everything.
        sel.remap(0, |_| None);
        assert_eq!(sel.lead(), None);
        assert_eq!(sel.count(), 0);
    }

    #[test]
    fn pointing_moves_the_cursor_and_keeps_the_selection() {
        let mut sel = Selection::new();
        sel.set_len(10);
        sel.click(0);
        sel.shift_extend(4);
        sel.point(1);
        assert_eq!(sel.lead(), Some(1));
        assert_eq!(sel.iter().collect::<Vec<_>>(), [0, 1, 2, 3, 4]);
        sel.point(99);
        assert_eq!(sel.lead(), Some(9), "clamped into the rows");
    }
}
