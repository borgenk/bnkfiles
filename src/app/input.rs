//! Input: what the keyboard and the pointer do to the view.
//!
//! The protocol events land here, are turned into intent ([`crate::keymap`] says
//! what a key *is*; this module says what it *does* in the view that has it), and
//! the view state changes. Nothing here draws: it marks the frame dirty and the
//! loop presents.

use std::time::{Duration, Instant};

use crate::app::{paint, Focus, State, ViewMode};
use crate::explorer::{contains, segment_at};
use crate::keymap::{self, Command, KeyAction};
use crate::platform::error::{Error, Result};
use crate::platform::ffi;
use crate::platform::geom::Rect;
use crate::platform::protocol::{
    wl_keyboard, wl_pointer, wl_seat, wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1,
};
use crate::platform::scroll;
use crate::platform::wire::{Arg, Reader};
use crate::prompt::{Prompt, TextField};
use crate::sidebar::Row;
use crate::unreadable::Action;
use crate::viewer::ZOOM_STEP;

use std::os::fd::AsRawFd;

/// The right mouse button (Linux input-event-codes `BTN_RIGHT`), which the
/// shared protocol table does not name.
const BTN_RIGHT: u32 = 0x111;
/// The thumb buttons that step through the history: `BTN_SIDE` and
/// `BTN_EXTRA` are what most mice send, and `BTN_BACK` and `BTN_FORWARD`
/// are what some others do.
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;
const BTN_FORWARD: u32 = 0x115;
const BTN_BACK: u32 = 0x116;

/// Accumulated scroll (in wl_fixed units) that makes one step. A classic wheel
/// notch is ~10-15, so this advances about one image per notch.
const SCROLL_NOTCH: f64 = 10.0;
/// Logical pixels the grid scrolls per wheel notch.
const GRID_SCROLL_STEP: f32 = 120.0;
/// Rows the detail list scrolls per wheel notch.
const LIST_SCROLL_ROWS: f32 = 3.0;
/// A second press within this long is a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// Key repeat, until the compositor's `repeat_info` says otherwise.
const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_REPEAT_INTERVAL: Duration = Duration::from_millis(40);
/// Repeats fired in one wake-up, so a stalled loop cannot burst-navigate a folder.
const MAX_REPEATS_PER_TICK: u32 = 8;
/// The left and right Alt keys' evdev codes, and the bit each sets in
/// `State::alt_keys`. Alt is read from the keys themselves because
/// `platform::xkb` reports only Shift and Ctrl.
const ALT_KEYS: [(u32, u8); 2] = [(56, 1), (100, 2)];

/// The held key that is repeating, and when it next fires.
pub(super) struct KeyRepeat {
    pub(super) key: Option<u32>,
    action: KeyAction,
    pub(super) next: Option<Instant>,
    delay: Duration,
    interval: Duration,
    enabled: bool,
}

impl Default for KeyRepeat {
    fn default() -> Self {
        Self {
            key: None,
            action: KeyAction::None,
            next: None,
            delay: DEFAULT_REPEAT_DELAY,
            interval: DEFAULT_REPEAT_INTERVAL,
            enabled: true,
        }
    }
}

impl State {
    // ---- seat ------------------------------------------------------------

    /// The seat's input capabilities changed. Acquire an object when its capability
    /// appears, and release it when the capability goes away, so an undock/redock (or
    /// any hot-plug) that drops then restores the keyboard or pointer leaves us
    /// driving a fresh, live object.
    pub(super) fn on_seat_capabilities(&mut self, caps: u32) {
        let seat = match self.seat {
            Some(seat) => seat,
            None => return,
        };
        let has_keyboard = caps & wl_seat::CAP_KEYBOARD != 0;
        let has_pointer = caps & wl_seat::CAP_POINTER != 0;

        if has_keyboard && self.keyboard == 0 {
            self.keyboard = self.alloc_id();
            self.conn
                .request(seat, wl_seat::GET_KEYBOARD, &[Arg::NewId(self.keyboard)]);
        } else if !has_keyboard && self.keyboard != 0 {
            // `release` arrived in version 3; before it the object is simply dropped.
            if self.seat_version >= 3 {
                self.conn.request(self.keyboard, wl_keyboard::RELEASE, &[]);
            }
            self.keyboard = 0;
            // A key held on a keyboard that is gone is not held any more, and the
            // modifiers we believe are down are stale.
            self.stop_repeat();
            self.xkb.clear_modifiers();
            self.xkb.reset_compose();
            self.alt_keys = 0;
        }

        if has_pointer && self.pointer == 0 {
            self.pointer = self.alloc_id();
            self.conn
                .request(seat, wl_seat::GET_POINTER, &[Arg::NewId(self.pointer)]);
            // Pair the pointer with a cursor-shape device, so the cursor over the
            // window is one we chose. Without the manager it keeps whatever the
            // compositor last set.
            if let Some(manager) = self.cursor_shape_manager {
                self.cursor_shape_device = self.alloc_id();
                self.conn.request(
                    manager,
                    wp_cursor_shape_manager_v1::GET_POINTER,
                    &[
                        Arg::NewId(self.cursor_shape_device),
                        Arg::Object(self.pointer),
                    ],
                );
            }
        } else if !has_pointer && self.pointer != 0 {
            // The cursor-shape device is bound to the pointer, so it goes first.
            if self.cursor_shape_device != 0 {
                self.conn.request(
                    self.cursor_shape_device,
                    wp_cursor_shape_device_v1::DESTROY,
                    &[],
                );
                self.cursor_shape_device = 0;
            }
            if self.seat_version >= 3 {
                self.conn.request(self.pointer, wl_pointer::RELEASE, &[]);
            }
            self.pointer = 0;
            // No pointer, so no shape set on it, no drag, and no thumb grabbed: none
            // of that can outlive the pointer, and a stale grab would otherwise seize
            // the next pointer's first motion.
            self.pointer_shape = 0;
            self.dragging = false;
            self.explorer_bar.release();
            self.pointer_in = false;
            self.hover = None;
        }
    }

    // ---- keyboard --------------------------------------------------------

    pub(super) fn on_keyboard(&mut self, opcode: u16, r: &mut Reader) -> Result<()> {
        match opcode {
            wl_keyboard::EV_KEYMAP => {
                let format = r.u32()?;
                let size = r.u32()? as usize;
                let fd = self
                    .conn
                    .take_fd()
                    .ok_or_else(|| Error::msg("wl_keyboard.keymap arrived without its fd"))?;
                // A compositor with no keymap to offer sends an empty one, and an
                // empty mapping is an error rather than no bytes.
                if size == 0 {
                    return Ok(());
                }
                let bytes = ffi::read_mapped(fd.as_raw_fd(), size)?;
                self.xkb.load_keymap(&bytes, format)?;
            }
            wl_keyboard::EV_KEY => {
                self.last_serial = r.u32()?;
                let _time = r.u32()?;
                let code = r.u32()?;
                let key_state = r.u32()?;
                if let Some(&(_, bit)) = ALT_KEYS.iter().find(|(alt, _)| *alt == code) {
                    if key_state == wl_keyboard::KEY_STATE_PRESSED {
                        self.alt_keys |= bit;
                    } else {
                        self.alt_keys &= !bit;
                    }
                }
                if key_state == wl_keyboard::KEY_STATE_PRESSED {
                    let action = keymap::action(&self.xkb, code);
                    // Armed before the action runs, so an action that ends the
                    // repeat (an activation, a prompt opening) has the last word.
                    self.start_repeat(code, action);
                    self.apply(action);
                } else if self.repeat.key == Some(code) {
                    self.stop_repeat();
                }
            }
            wl_keyboard::EV_MODIFIERS => {
                self.last_serial = r.u32()?;
                let depressed = r.u32()?;
                let latched = r.u32()?;
                let locked = r.u32()?;
                let group = r.u32()?;
                self.xkb.update_modifiers(depressed, latched, locked, group);
            }
            wl_keyboard::EV_REPEAT_INFO => {
                let rate = r.u32()? as i32;
                let delay = r.u32()? as i32;
                self.repeat.enabled = rate > 0;
                if rate > 0 {
                    self.repeat.interval = Duration::from_millis((1000 / rate as u64).max(1));
                } else {
                    self.stop_repeat();
                }
                if delay >= 0 {
                    self.repeat.delay = Duration::from_millis(delay as u64);
                }
            }
            wl_keyboard::EV_ENTER => {
                self.last_serial = r.u32()?;
                let _surface = r.u32()?;
                // The compositor lists the keys already held as focus arrives. We do
                // not synthesize presses from them: a key held down through an
                // alt-tab should not fire its action, nor start repeating, until it
                // is pressed again while we have focus. A held Alt is the exception:
                // it fires nothing by itself, and Alt+Left after an Alt-Tab back
                // should mean back, not up.
                let held = r.array()?;
                self.alt_keys = 0;
                for code in held.as_chunks::<4>().0 {
                    let code = u32::from_le_bytes(*code);
                    if let Some(&(_, bit)) = ALT_KEYS.iter().find(|(alt, _)| *alt == code) {
                        self.alt_keys |= bit;
                    }
                }
                // Nothing carries over from before we had focus.
                self.stop_repeat();
            }
            wl_keyboard::EV_LEAVE => {
                // Focus left: the held key is not held any more, as far as we can
                // ever know, the modifiers we think are down are stale, and the key
                // that would finish a dead-key sequence is going somewhere else.
                self.stop_repeat();
                self.xkb.clear_modifiers();
                self.xkb.reset_compose();
                self.alt_keys = 0;
            }
            _ => {}
        }
        Ok(())
    }

    fn start_repeat(&mut self, code: u32, action: KeyAction) {
        if !self.repeat.enabled || !action.repeats() {
            // A new press that does not repeat still cancels the one that was:
            // otherwise the old key keeps firing under the new one.
            self.stop_repeat();
            return;
        }
        self.repeat.key = Some(code);
        self.repeat.action = action;
        self.repeat.next = Some(Instant::now() + self.repeat.delay);
    }

    pub(super) fn stop_repeat(&mut self) {
        self.repeat.key = None;
        self.repeat.next = None;
    }

    /// Fire any repeats now due. Capped per turn, so a stall cannot unleash a burst
    /// of navigation; the schedule resyncs from now when it is capped.
    pub(super) fn fire_repeat(&mut self) {
        if self.repeat.key.is_none() {
            return;
        }
        let action = self.repeat.action;
        let now = Instant::now();
        let mut fired = 0;
        while let Some(next) = self.repeat.next {
            if next > now || fired >= MAX_REPEATS_PER_TICK {
                break;
            }
            self.apply(action);
            // The action may have ended the repeat itself.
            if self.repeat.key.is_none() {
                return;
            }
            self.repeat.next = Some(next + self.repeat.interval);
            fired += 1;
        }
        if fired >= MAX_REPEATS_PER_TICK {
            self.repeat.next = Some(now + self.repeat.interval);
        }
    }

    /// Act on a key. The chorded commands go first (Ctrl+D is a pin, not the debug
    /// overlay), then the keys that mean the same in every view, then the rest is
    /// the active view's to interpret.
    fn apply(&mut self, action: KeyAction) {
        // A prompt is modal. While one is up it takes every key, so a stray
        // letter cannot toggle the grid or quit the window from behind a rename
        // box, and the worker waiting on a conflict gets its answer.
        if self.prompt.is_some() {
            self.prompt_key(action);
            return;
        }
        // So is the command list.
        if self.menu.is_some() {
            self.menu_key(action);
            return;
        }
        // The filter field takes the keys while it has the focus, for the same
        // reason: typing "q" into it must not quit. It keeps them if a
        // narrowed header hides the field; the status bar shows the text then.
        // Chords stay commands, and Escape is the help overlay's or the running
        // job's first, as it is everywhere else.
        let escape_elsewhere =
            action == KeyAction::Escape && (self.show_help || self.jobs.is_busy());
        if self.focus == Focus::Filter
            && self.mode.is_explorer()
            && !self.xkb.ctrl_active()
            && !escape_elsewhere
        {
            self.filter_key(action);
            return;
        }
        // Escape calls off a running job. It is the only thing on screen with a
        // progress line, so while one is there that is what Escape is about.
        if action == KeyAction::Escape && self.jobs.is_busy() {
            self.jobs.cancel();
            return;
        }
        if action == KeyAction::Escape && self.show_help {
            self.show_help = false;
            self.dirty = true;
            return;
        }
        // A narrowed window can take the inspector away from under the focus.
        // Settled before the table, which reads where the keyboard is.
        if self.focus == Focus::Inspector && self.regions().inspector.w == 0 {
            self.focus = Focus::Entries;
        }
        // Then the binding table: every key that is a command, wherever the
        // keyboard is. What is left is the active view's movement keys.
        if let Some(command) = keymap::command(action, self.mods(), self.key_context()) {
            self.run_command(command);
            return;
        }
        match self.mode {
            ViewMode::Viewer => self.viewer_key(action),
            _ => self.explorer_key(action),
        }
    }

    fn viewer_key(&mut self, action: KeyAction) {
        match action {
            KeyAction::Left => self.navigate(-1),
            KeyAction::Right => self.navigate(1),
            KeyAction::Escape => self.leave_viewer(),
            _ => {}
        }
    }

    /// The explorer's keys, dispatched to whichever pane has the focus.
    fn explorer_key(&mut self, action: KeyAction) {
        if action == KeyAction::Tab {
            self.cycle_focus(self.xkb.shift_active());
            return;
        }
        match self.focus {
            Focus::Entries => self.entries_key(action),
            Focus::Sidebar => self.sidebar_key(action),
            Focus::Path => self.path_key(action),
            Focus::Inspector => self.inspector_key(action),
            // Taken by `apply` before the view sees it.
            Focus::Filter => {}
        }
    }

    /// The filter field: the editing keys edit it, and the rest leave it.
    /// Enter goes to the first match, Escape empties the field and goes back
    /// to the entries.
    fn filter_key(&mut self, action: KeyAction) {
        match action {
            KeyAction::Tab => self.cycle_focus(self.xkb.shift_active()),
            KeyAction::Menu => self.run_command(keymap::Command::Commands),
            KeyAction::Enter | KeyAction::Down => {
                self.focus = Focus::Entries;
                if self.filter.is_active() && !self.rows.is_empty() {
                    self.sel.click(0);
                    self.reveal_lead();
                }
                self.dirty = true;
            }
            KeyAction::Escape => {
                self.clear_filter();
                self.focus = Focus::Entries;
                self.dirty = true;
            }
            KeyAction::Left => self.edit_filter(TextField::left),
            KeyAction::Right => self.edit_filter(TextField::right),
            KeyAction::Home => self.edit_filter(TextField::home),
            KeyAction::End => self.edit_filter(TextField::end),
            KeyAction::Back => self.edit_filter(TextField::backspace),
            KeyAction::Delete => self.edit_filter(TextField::delete),
            KeyAction::Char(c) => self.edit_filter(|field| field.insert(c)),
            _ => {}
        }
    }

    /// The entry list or grid. Shift extends the selection wherever the cursor
    /// moves, which is the one modifier the keyboard needs here; Ctrl-toggling a
    /// single row is a pointer gesture.
    fn entries_key(&mut self, action: KeyAction) {
        let extending = self.xkb.shift_active();
        // In the grid, up and down cross to the row of cells above or below; in
        // the list they are one entry, and left and right walk the tree instead.
        let last = self.sel.len().saturating_sub(1) as i32;
        match action {
            KeyAction::Up => self.move_rows(-1, extending),
            KeyAction::Down => self.move_rows(1, extending),
            KeyAction::Left if self.mode == ViewMode::Grid => self.move_across(-1, extending),
            KeyAction::Right if self.mode == ViewMode::Grid => self.move_across(1, extending),
            // In the list the horizontal arrows walk the tree, which is what a
            // one-column list has the room to mean by them.
            KeyAction::Left => self.go_parent(),
            KeyAction::Right => self.activate_lead(),
            KeyAction::Home => self.move_lead(-last, extending),
            KeyAction::End => self.move_lead(last, extending),
            KeyAction::PageUp => {
                let page = self.page_rows();
                self.move_rows(-page, extending);
            }
            KeyAction::PageDown => {
                let page = self.page_rows();
                self.move_rows(page, extending);
            }
            // Escape gives up the selection first, and only quits from an empty
            // one: a stray Escape over a hundred selected files should not be the
            // same key as the one that closes the window.
            // A filter goes next, so the rows come back before the window goes.
            // A notice still up is the last thing before the window: an Escape
            // meant to wave it away must not close everything.
            KeyAction::Escape => {
                if self.sel.count() > 0 {
                    self.sel.clear();
                    self.dirty = true;
                } else if !self.clear_filter() && !self.dismiss_notice() {
                    self.closed = true;
                }
            }
            _ => {}
        }
    }

    /// The keys that go to the prompt on screen, while one is.
    ///
    /// Enter and Escape mean the same thing to all three (do it, or do not), so
    /// they are answered before the prompt is borrowed to be edited. What the
    /// rest mean depends: a rename is a text field and the arrows walk the
    /// cursor, while the other two are a row of answers and the arrows walk
    /// those.
    fn prompt_key(&mut self, action: KeyAction) {
        match action {
            KeyAction::Enter => return self.answer_prompt(true),
            KeyAction::Escape => return self.answer_prompt(false),
            _ => {}
        }
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        match prompt {
            Prompt::Rename { field, .. } => match action {
                KeyAction::Left => field.left(),
                KeyAction::Right => field.right(),
                KeyAction::Home => field.home(),
                KeyAction::End => field.end(),
                KeyAction::Back => field.backspace(),
                KeyAction::Delete => field.delete(),
                KeyAction::Char(c) => field.insert(c),
                _ => {}
            },
            _ => match action {
                KeyAction::Left => prompt.move_choice(-1),
                KeyAction::Right => prompt.move_choice(1),
                // On a conflict, A makes the answer stand for the whole job
                // rather than being asked once per file.
                KeyAction::Char('a') | KeyAction::Char('A') => {
                    if let Prompt::Conflict { all, .. } = prompt {
                        *all = !*all;
                    }
                }
                _ => {}
            },
        }
        self.dirty = true;
    }

    /// The sidebar: the arrows walk its rows, headers included. On a header,
    /// Enter folds or unfolds it, Right unfolds and Left folds, as in a tree;
    /// on a place, Enter or Right goes there. Left from a place, or from a
    /// header already folded, hands the keyboard back to the entries.
    fn sidebar_key(&mut self, action: KeyAction) {
        let rows = self.sidebar_rows();
        let last = rows.len().saturating_sub(1);
        let here = rows.get(self.place_sel).copied();
        match (action, here) {
            (KeyAction::Up, _) => self.place_sel = self.place_sel.saturating_sub(1),
            (KeyAction::Down, _) => self.place_sel = (self.place_sel + 1).min(last),
            (KeyAction::Home, _) => self.place_sel = 0,
            (KeyAction::End, _) => self.place_sel = last,
            (KeyAction::Enter, _) => self.activate_sidebar_row(self.place_sel),
            (KeyAction::Right, Some(Row::Header(section, true)))
            | (KeyAction::Left, Some(Row::Header(section, false))) => self.toggle_section(section),
            (KeyAction::Right, Some(Row::Place(_))) => self.activate_sidebar_row(self.place_sel),
            (KeyAction::Right, _) => {}
            (KeyAction::Escape | KeyAction::Left, _) => self.focus = Focus::Entries,
            _ => return,
        }
        self.dirty = true;
    }

    /// The inspector is read-only, so the only keys it has hand the focus back.
    fn inspector_key(&mut self, action: KeyAction) {
        if matches!(action, KeyAction::Escape | KeyAction::Left) {
            self.focus = Focus::Entries;
            self.dirty = true;
        }
    }

    fn path_key(&mut self, action: KeyAction) {
        let last = self.path_segments().len().saturating_sub(1);
        match action {
            KeyAction::Left => self.path_sel = self.path_sel.saturating_sub(1),
            KeyAction::Right => self.path_sel = (self.path_sel + 1).min(last),
            KeyAction::Home => self.path_sel = 0,
            KeyAction::End => self.path_sel = last,
            KeyAction::Enter => self.open_path_segment(self.path_sel),
            KeyAction::Escape => self.focus = Focus::Entries,
            _ => return,
        }
        self.dirty = true;
    }

    /// Move the keyboard between the panes: the entries, the sidebar, the path,
    /// the filter field and the inspector, each while it is shown. Backwards on
    /// Shift, so a Tab overshoot is one keystroke to undo rather than two more
    /// to go round.
    fn cycle_focus(&mut self, backwards: bool) {
        let all = [
            Focus::Entries,
            Focus::Sidebar,
            Focus::Path,
            Focus::Filter,
            Focus::Inspector,
        ];
        let inspector = self.regions().inspector.w > 0;
        // The field is a stop while it is drawn, or while it holds the keys.
        let field = self.header_layout().field.is_some() || self.focus == Focus::Filter;
        let order: Vec<Focus> = all
            .into_iter()
            .filter(|f| match f {
                Focus::Inspector => inspector,
                Focus::Filter => field,
                _ => true,
            })
            .collect();
        let at = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if backwards {
            (at + order.len() - 1) % order.len()
        } else {
            (at + 1) % order.len()
        };
        self.focus = order[next];
        // Arriving at a pane, start from where it already is rather than the top:
        // the sidebar opens on the place standing for this folder, and the path on
        // the segment naming it.
        match self.focus {
            Focus::Sidebar => {
                if let Some(here) = self.sidebar_row_here() {
                    self.place_sel = here;
                }
            }
            Focus::Path => self.path_sel = self.path_segments().len().saturating_sub(1),
            Focus::Entries | Focus::Filter | Focus::Inspector => {}
        }
        self.dirty = true;
    }

    // ---- zoom ------------------------------------------------------------

    pub(super) fn zoom(&mut self, factor: f32, at_cursor: bool) {
        let Some((w, h, iw, ih)) = self.view_dims() else {
            return;
        };
        let focus = self.focus(at_cursor, w, h);
        self.viewer.zoom_by(factor, focus, w, h, iw, ih);
        self.dirty = true;
    }

    pub(super) fn view_fit(&mut self) {
        self.viewer.set_fit();
        self.dirty = true;
    }

    pub(super) fn view_actual(&mut self) {
        if let Some((w, h, iw, ih)) = self.view_dims() {
            let focus = self.focus(false, w, h);
            self.viewer.set_actual(focus, w, h, iw, ih);
        } else {
            self.viewer.set_fit();
        }
        self.dirty = true;
    }

    fn view_toggle(&mut self) {
        let Some((w, h, iw, ih)) = self.view_dims() else {
            return;
        };
        let focus = self.focus(true, w, h);
        self.viewer.toggle(focus, w, h, iw, ih);
        self.dirty = true;
    }

    /// The zoom focus in canvas coordinates: the cursor, or the centre of the
    /// canvas. The pointer is in window coordinates, so it is moved by the
    /// canvas's origin, which the framed viewer puts beside the sidebar and
    /// under the header.
    fn focus(&self, at_cursor: bool, w: u32, h: u32) -> (f32, f32) {
        if at_cursor {
            let (ox, oy) = self.canvas().map_or((0, 0), |c| (c.x, c.y));
            (
                self.pointer_pos.0 as f32 - ox as f32,
                self.pointer_pos.1 as f32 - oy as f32,
            )
        } else {
            (w as f32 / 2.0, h as f32 / 2.0)
        }
    }

    // ---- pointer ---------------------------------------------------------

    pub(super) fn on_pointer(&mut self, opcode: u16, r: &mut Reader) -> Result<()> {
        match opcode {
            wl_pointer::EV_ENTER => {
                self.pointer_enter_serial = r.u32()?;
                let _surface = r.u32()?;
                // Surface-local coordinates are logical; everything here is device.
                self.pointer_pos = (
                    self.to_dev(r.fixed()? as f64),
                    self.to_dev(r.fixed()? as f64),
                );
                // The compositor gives an entering pointer no shape at all, so it
                // keeps whatever the last window set. Claim it.
                self.pointer_shape = 0;
                self.pointer_in = true;
                self.apply_cursor();
                self.update_hover();
            }
            wl_pointer::EV_LEAVE => {
                // A drag cannot continue over a window we are no longer in, and
                // no button is under a pointer that is not here.
                self.pointer_in = false;
                if self.hover.take().is_some() {
                    self.dirty = true;
                }
                self.dragging = false;
                self.explorer_bar.release();
                self.explorer_bar.set_near(false);
            }
            wl_pointer::EV_MOTION => {
                let _time = r.u32()?;
                let (x, y) = (
                    self.to_dev(r.fixed()? as f64),
                    self.to_dev(r.fixed()? as f64),
                );
                self.on_motion(x, y);
            }
            wl_pointer::EV_BUTTON => {
                self.last_serial = r.u32()?;
                let _time = r.u32()?;
                let button = r.u32()?;
                let state = r.u32()?;
                let pressed = state == wl_pointer::BUTTON_STATE_PRESSED;
                if button == wl_pointer::BTN_LEFT {
                    self.on_left_button(pressed);
                } else if pressed {
                    match button {
                        BTN_RIGHT => self.on_right_press(),
                        BTN_SIDE | BTN_BACK => self.on_history_button(Command::Back),
                        BTN_EXTRA | BTN_FORWARD => self.on_history_button(Command::Forward),
                        _ => {}
                    }
                }
            }
            wl_pointer::EV_AXIS => {
                let _time = r.u32()?;
                let axis = r.u32()?;
                let value = r.fixed()?;
                if axis == wl_pointer::AXIS_VERTICAL_SCROLL {
                    self.scroll_v += value as f64;
                }
            }
            wl_pointer::EV_AXIS_DISCRETE => {
                let axis = r.u32()?;
                let discrete = r.u32()? as i32;
                if axis == wl_pointer::AXIS_VERTICAL_SCROLL {
                    self.scroll_notches += discrete;
                }
            }
            wl_pointer::EV_AXIS_SOURCE => {
                self.scroll_source = r.u32()?;
            }
            wl_pointer::EV_AXIS_STOP => {
                // The gesture ended: whatever fraction of a notch it left over
                // belongs to it, not to the next flick.
                self.scroll_accum = 0.0;
            }
            wl_pointer::EV_FRAME => self.commit_scroll(),
            _ => {}
        }
        Ok(())
    }

    /// A pointer coordinate, from the compositor's logical surface space into the
    /// device pixels everything here is laid out in.
    fn to_dev(&self, logical: f64) -> f64 {
        logical * self.scaling.factor_120 as f64 / 120.0
    }

    fn on_motion(&mut self, x: f64, y: f64) {
        let (dx, dy) = (x - self.pointer_pos.0, y - self.pointer_pos.1);
        self.pointer_pos = (x, y);

        // A live scrollbar drag owns the pointer.
        if self.dragging_bar() {
            self.drag_bar(y as i32);
            return;
        }
        if self.dragging {
            if let Some((w, h, iw, ih)) = self.view_dims() {
                self.viewer.pan_by(dx as f32, dy as f32, w, h, iw, ih);
                self.dirty = true;
            }
            return;
        }
        self.update_hover();
        // The bar grows and stays lit while the pointer is near its lane.
        if self.mode.is_explorer() {
            let near = self.bar_near(x, y);
            if self.explorer_bar.set_near(near) {
                self.dirty = true;
            }
        }
        self.apply_cursor();
    }

    fn on_left_button(&mut self, pressed: bool) {
        if !pressed {
            self.dragging = false;
            self.explorer_bar.release();
            self.apply_cursor();
            return;
        }
        // A prompt is modal for the pointer too: a click behind "Delete 3 items for
        // good?" must not move the selection the question was asked about.
        if self.prompt.is_some() {
            return;
        }
        if self.menu.is_some() {
            let (px, py) = (self.pointer_pos.0 as f32, self.pointer_pos.1 as f32);
            self.menu_click(px, py);
            return;
        }
        // A press in the scrollbar's lane grabs its thumb, whatever the view holds.
        if self.mode.is_explorer()
            && self.bar_near(self.pointer_pos.0, self.pointer_pos.1)
            && self.press_bar()
        {
            self.apply_cursor();
            return;
        }
        let (px, py) = (self.pointer_pos.0 as f32, self.pointer_pos.1 as f32);
        // The header's buttons mean the same thing however the view got here.
        if let Some(control) = self.header_layout().control_at(px, py) {
            self.last_click = None;
            self.press_control(control);
            return;
        }
        match self.mode {
            ViewMode::Viewer => {
                // In the framed viewer the sidebar still goes places. A press on
                // the header or anywhere else off the picture's canvas starts no
                // pan: the drag belongs to the picture.
                let rows = self.sidebar_rows();
                if let Some(row) = self.explorer_layout().place_at(rows.len(), px, py) {
                    // A header folds where it is; a place leaves the picture
                    // for the folder.
                    if let Some(Row::Place(_)) = rows.get(row) {
                        self.leave_viewer();
                        self.focus = Focus::Sidebar;
                    }
                    self.place_sel = row;
                    self.activate_sidebar_row(row);
                    return;
                }
                if !self.canvas().is_some_and(|c| contains(c, px, py)) {
                    return;
                }
                let now = Instant::now();
                let double = self
                    .last_click
                    .is_some_and(|t| now.duration_since(t) < DOUBLE_CLICK);
                if double {
                    self.last_click = None;
                    self.dragging = false;
                    self.view_toggle();
                } else {
                    self.last_click = Some(now);
                    self.dragging = true;
                }
                self.apply_cursor();
            }
            _ => self.explorer_click(px, py),
        }
    }

    /// The right button opens the command list where it was pressed: on an
    /// entry, about that entry, which becomes the selection unless it was
    /// already part of it, as in any file manager.
    fn on_right_press(&mut self) {
        if self.prompt.is_some() {
            return;
        }
        self.menu = None;
        self.dragging = false;
        self.explorer_bar.release();
        let (px, py) = (self.pointer_pos.0 as f32, self.pointer_pos.1 as f32);
        if self.mode.is_explorer() {
            if let Some(row) = self.entry_at(px, py) {
                // Already selected: the selection stays, and the cursor goes
                // to the row clicked, which is what Open and Rename act on.
                if self.sel.is_selected(row) {
                    self.sel.point(row);
                } else {
                    self.sel.click(row);
                }
                self.focus = Focus::Entries;
            } else if contains(self.regions().content, px, py) {
                self.focus = Focus::Entries;
            }
        }
        self.open_menu(crate::menu::Anchor::At(px as i32, py as i32));
    }

    /// A thumb button steps through the history, as Alt+Left and Alt+Right
    /// do, unless a prompt or the command list is up and holds the pointer.
    fn on_history_button(&mut self, command: Command) {
        if self.prompt.is_some() || self.menu.is_some() {
            return;
        }
        self.dragging = false;
        self.explorer_bar.release();
        self.run_command(command);
    }

    /// The entry row at window point `(px, py)`, in whichever layout shows.
    fn entry_at(&self, px: f32, py: f32) -> Option<usize> {
        let layout = self.explorer_layout();
        match self.mode {
            ViewMode::Grid => {
                let content = layout.content;
                self.grid_layout()
                    .hit(
                        self.rows.len(),
                        px - content.x as f32,
                        py - content.y as f32,
                    )
                    .filter(|_| contains(content, px, py))
            }
            ViewMode::List => layout.row_at(self.rows.len(), px, py),
            ViewMode::Viewer => None,
        }
    }

    /// A click in the explorer, dispatched by where it landed. Clicking a pane also
    /// gives it the keyboard, so the pointer and the keyboard never disagree about
    /// which pane is being worked in.
    fn explorer_click(&mut self, px: f32, py: f32) {
        let layout = self.explorer_layout();

        if self.header_layout().on_field(px, py) {
            self.focus = Focus::Filter;
            self.dirty = true;
            return;
        }
        // A heading sorts by its column, and the other way on a second click.
        if let Some(key) = layout.heading_at(px, py) {
            self.focus = Focus::Entries;
            self.set_order(self.order.by(key));
            return;
        }
        // The breadcrumb and the sidebar navigate on a single click: they hold a
        // handful of destinations, not a selection to build up.
        let segments = self.path_segments();
        if let Some(i) = segment_at(&segments, px, py) {
            self.focus = Focus::Path;
            self.path_sel = i;
            self.open_path_segment(i);
            return;
        }
        if let Some(row) = layout.place_at(self.sidebar_rows().len(), px, py) {
            self.focus = Focus::Sidebar;
            self.place_sel = row;
            self.activate_sidebar_row(row);
            return;
        }
        if contains(self.regions().inspector, px, py) {
            self.focus = Focus::Inspector;
            self.dirty = true;
            return;
        }

        // A folder that could not be read offers what can be done instead,
        // each a key and a click alike.
        if let Some(action) = self.unreadable_panel().and_then(|p| p.action_at(px, py)) {
            self.focus = Focus::Entries;
            self.run_command(match action {
                Action::Up => Command::Up,
                Action::Back => Command::Back,
                Action::Retry => Command::Reload,
            });
            return;
        }

        // The grid's own hit test rejects the gaps between cells, but a click
        // there is still a click in the entry pane.
        let Some(row) = self.entry_at(px, py) else {
            // A click in the entry pane but on no row: take the focus, and let go
            // of the selection, which is how a file manager says "never mind".
            if contains(layout.content, px, py) {
                self.focus = Focus::Entries;
                self.sel.clear();
                self.dirty = true;
            }
            return;
        };
        self.focus = Focus::Entries;

        // Ctrl adds and removes; Shift takes the run from the anchor; a plain click
        // replaces the selection, and a second one on the same row opens it.
        if self.xkb.ctrl_active() {
            self.sel.ctrl_toggle(row);
        } else if self.xkb.shift_active() {
            self.sel.shift_extend(row);
        } else {
            let now = Instant::now();
            let again = self.sel.lead() == Some(row)
                && self
                    .last_click
                    .is_some_and(|t| now.duration_since(t) < DOUBLE_CLICK);
            self.sel.click(row);
            if again {
                self.last_click = None;
                self.activate_lead();
                return;
            }
            self.last_click = Some(now);
        }
        self.dirty = true;
    }

    /// Apply a pointer frame's worth of scroll.
    ///
    /// A discrete wheel gives exact notch counts (one step each, no drift); a
    /// continuous source (a touchpad) falls back to the accumulator. Committing per
    /// frame means a multi-notch flick renders only its final image, not every step
    /// on the way.
    fn commit_scroll(&mut self) {
        // wl_pointer.frame also ends motion and button groups; those are not scroll.
        if self.scroll_notches == 0 && self.scroll_v == 0.0 {
            return;
        }
        // The wheel is as modal as the keys: nothing scrolls behind a question,
        // and over the command list it walks the list, so every row of a long
        // one is in reach of the mouse.
        if let Some(menu) = self.menu.as_mut() {
            let step = self.scroll_notches.signum();
            if step != 0 {
                menu.move_cursor(step);
                self.dirty = true;
            }
            self.scroll_notches = 0;
            self.scroll_v = 0.0;
            return;
        }
        if self.prompt.is_some() {
            self.scroll_notches = 0;
            self.scroll_v = 0.0;
            return;
        }
        // One detent is one step, by the *direction* of the notch count and not its
        // magnitude: the compositor signals a detent crossing once per physical
        // notch, but the count itself can jitter (a fractional scroll-speed factor
        // applied to v120 makes it read 1,2,2,2). libinput emits one event, and so
        // one frame, per detent, so a fast flick is still many detents.
        let step = self.scroll_notches.signum();
        let continuous = matches!(
            self.scroll_source,
            wl_pointer::AXIS_SOURCE_FINGER | wl_pointer::AXIS_SOURCE_CONTINUOUS
        );
        let scale = self.scale();
        match self.mode {
            ViewMode::Grid => {
                let step_px = scale.px(GRID_SCROLL_STEP as i32) as f32;
                if step != 0 {
                    self.scroll_explorer(step as f32 * step_px);
                } else if continuous {
                    self.scroll_explorer(self.scroll_v as f32 * scale.px(4) as f32);
                }
            }
            ViewMode::List => {
                let row = self.explorer_layout().row as f32;
                if step != 0 {
                    self.scroll_explorer(step as f32 * row * LIST_SCROLL_ROWS);
                } else if continuous {
                    self.scroll_explorer(self.scroll_v as f32 * scale.px(2) as f32);
                }
            }
            // Off the canvas, over the framed viewer's sidebar or header, the
            // wheel does nothing, as a press there does nothing: a zoom there
            // would be anchored at a point that is not on the picture.
            ViewMode::Viewer
                if !self.canvas().is_some_and(|c| {
                    contains(c, self.pointer_pos.0 as f32, self.pointer_pos.1 as f32)
                }) => {}
            ViewMode::Viewer => {
                let ctrl = self.xkb.ctrl_active();
                if step != 0 {
                    self.apply_viewer_scroll(step, ctrl);
                    self.scroll_accum = 0.0;
                } else if continuous {
                    self.scroll_accum += self.scroll_v;
                    while self.scroll_accum >= SCROLL_NOTCH {
                        self.scroll_accum -= SCROLL_NOTCH;
                        self.apply_viewer_scroll(1, ctrl);
                    }
                    while self.scroll_accum <= -SCROLL_NOTCH {
                        self.scroll_accum += SCROLL_NOTCH;
                        self.apply_viewer_scroll(-1, ctrl);
                    }
                }
            }
        }
        self.scroll_v = 0.0;
        self.scroll_notches = 0;
        // scroll_source persists: the compositor re-sends it only when it changes.
    }

    /// `steps` notches, signed: scrolling down is the next image, or (with Ctrl) a
    /// zoom out at the cursor.
    fn apply_viewer_scroll(&mut self, steps: i32, ctrl: bool) {
        let dir = steps.signum();
        for _ in 0..steps.unsigned_abs() {
            if ctrl {
                let factor = if dir > 0 { 1.0 / ZOOM_STEP } else { ZOOM_STEP };
                self.zoom(factor, true);
            } else {
                self.navigate(dir);
            }
        }
    }

    // ---- scrollbar -------------------------------------------------------

    fn dragging_bar(&self) -> bool {
        self.explorer_bar.grab().is_some()
    }

    /// Whether `(x, y)` is within the bar's proximity lane at the content's right
    /// edge: beside the track, not above it in the header, where the layout
    /// button sits in the same strip.
    fn bar_near(&self, x: f64, y: f64) -> bool {
        let Some(content) = self.scrolled_rect() else {
            return false;
        };
        let Some(track) = self.bar_track() else {
            return false;
        };
        y >= content.y as f64
            && y < (content.y + content.h) as f64
            && x >= (track.x + track.w - self.scale().px(paint::BAR_LANE)) as f64
            // Not past the content's right edge: the inspector may be there.
            && x < (content.x + content.w) as f64
    }

    /// The scrollable content, viewport, and scroll of the active layout, in device
    /// pixels. `None` in the viewer, which does not scroll.
    fn bar_extent(&self) -> Option<(i32, i32, i32)> {
        let content = self.scrolled_rect()?;
        Some(match self.mode {
            ViewMode::Grid => {
                let layout = self.grid_layout();
                (
                    layout.content_height(content.h),
                    content.h,
                    layout.scroll as i32,
                )
            }
            _ => {
                let layout = self.explorer_layout();
                (layout.content_height, content.h, layout.scroll as i32)
            }
        })
    }

    /// Grab the thumb under the pointer. Returns whether it took it: a press in the
    /// lane but off the thumb is not a grab (and falls through to the view).
    fn press_bar(&mut self) -> bool {
        let (Some((content, viewport, scroll)), Some(track)) =
            (self.bar_extent(), self.bar_track())
        else {
            return false;
        };
        let Some(thumb) = scroll::thumb(
            track,
            viewport,
            content,
            scroll,
            self.scale().px(paint::BAR_MIN_THUMB),
        ) else {
            return false;
        };
        let y = self.pointer_pos.1 as i32;
        if y < thumb.y || y >= thumb.y + thumb.h {
            return false;
        }
        // Remember where inside the thumb it was taken, so it tracks the pointer
        // instead of snapping its top to it.
        self.explorer_bar.press(y - thumb.y);
        self.dirty = true;
        true
    }

    /// Carry a live thumb drag: the scroll that puts the thumb's top where the
    /// pointer (less the grab offset) now is.
    fn drag_bar(&mut self, y: i32) {
        let (Some((content, viewport, _)), Some(track)) = (self.bar_extent(), self.bar_track())
        else {
            return;
        };
        let (Some(grab), Some(thumb_h)) = (self.explorer_bar.grab(), self.thumb_height()) else {
            return;
        };
        let scroll = scroll::scroll_at_thumb(track, thumb_h, viewport, content, y - grab) as f32;
        match self.mode {
            ViewMode::Grid => self.grid_scroll = scroll,
            _ => self.list_scroll = scroll,
        }
        self.dirty = true;
    }

    fn thumb_height(&self) -> Option<i32> {
        let (content, viewport, scroll) = self.bar_extent()?;
        let track = self.bar_track()?;
        scroll::thumb(
            track,
            viewport,
            content,
            scroll,
            self.scale().px(paint::BAR_MIN_THUMB),
        )
        .map(|t| t.h)
    }

    /// What the explorer's scrollbar scrolls: the grid's whole region, or the
    /// list's rows below its headings. Every measure of the bar (its track,
    /// its lane, its viewport) comes from this one rectangle, so the thumb
    /// that is drawn is the thumb that is grabbed.
    fn scrolled_rect(&self) -> Option<Rect> {
        let content = self.content_rect()?;
        Some(match self.mode {
            ViewMode::List => self.explorer_layout().content,
            _ => content,
        })
    }

    /// The scrollbar's track: a strip down the right edge of the entry region, not
    /// of the window, so it does not overlap the header or the status bar.
    pub(super) fn bar_track(&self) -> Option<Rect> {
        let content = self.scrolled_rect()?;
        let scale = self.scale();
        let width = scale.px(paint::BAR_WIDTH);
        let margin = scale.px(paint::BAR_MARGIN);
        Some(Rect {
            x: content.x + content.w - width - margin,
            y: content.y + margin,
            w: width,
            h: (content.h - 2 * margin).max(1),
        })
    }

    // ---- cursor ----------------------------------------------------------

    /// Light the header button under the pointer, and repaint only when that
    /// changes: a motion event per pixel must not be a frame per pixel.
    pub(super) fn update_hover(&mut self) {
        let (px, py) = (self.pointer_pos.0 as f32, self.pointer_pos.1 as f32);
        let hover = if self.pointer_in && self.configured {
            self.header_layout().control_at(px, py)
        } else {
            None
        };
        if hover != self.hover {
            self.hover = hover;
            self.dirty = true;
        }
    }

    /// The shape the cursor should have where it is now, sent only when it changes
    /// (a shape request per motion event would be a request per pixel moved).
    fn apply_cursor(&mut self) {
        let shape = if self.dragging || self.dragging_bar() {
            // Nothing in the protocol's v1 shape list is a closed hand, so the
            // grabbing cursor is the pointing hand: it at least says "this is
            // yours to move" rather than leaving the arrow on it.
            wp_cursor_shape_device_v1::SHAPE_POINTER
        } else {
            wp_cursor_shape_device_v1::SHAPE_DEFAULT
        };
        if shape == self.pointer_shape || self.cursor_shape_device == 0 {
            return;
        }
        self.pointer_shape = shape;
        let serial = self.pointer_enter_serial;
        self.conn.request(
            self.cursor_shape_device,
            wp_cursor_shape_device_v1::SET_SHAPE,
            &[Arg::Uint(serial), Arg::Uint(shape)],
        );
    }
}
