//! Commands: running what [`crate::keymap`]'s table names, and the command
//! list that offers the same table as a menu.
//!
//! A key press and a click in the list arrive at [`State::run_command`] alike,
//! so the two cannot behave differently, and whether a command can run now is
//! answered once, for both the list (which dims what cannot) and the key.

use crate::app::{Focus, State, ViewMode, FONT_STATUS};
use crate::header::Control;
use crate::keymap::{self, Command, Context, KeyAction, Mods};
use crate::menu::{Anchor, Menu, MenuLayout};
use crate::platform::geom::Rect;
use crate::prompt::TextField;
use crate::viewer::ZOOM_STEP;

impl State {
    /// Where the keyboard is, as the binding table reads it.
    pub(super) fn key_context(&self) -> Context {
        Context {
            viewer: self.mode == ViewMode::Viewer,
            entries: self.focus == Focus::Entries,
        }
    }

    /// The modifiers held now. Alt is read from the keys themselves, as the
    /// xkb state does not track it.
    pub(super) fn mods(&self) -> Mods {
        Mods {
            ctrl: self.xkb.ctrl_active(),
            shift: self.xkb.shift_active(),
            alt: self.alt_keys != 0,
        }
    }

    /// Whether `command` would do anything now. The list draws one that
    /// would not dimmed, and neither it nor its key runs it.
    pub(super) fn command_enabled(&self, command: Command) -> bool {
        let explorer = self.mode.is_explorer();
        match command {
            Command::Back => self.control_enabled(Control::Back),
            Command::Forward => self.control_enabled(Control::Forward),
            Command::Up => explorer && self.control_enabled(Control::Up),
            Command::Inspector => explorer || self.viewer_framed,
            Command::Open | Command::Rename => explorer && self.lead_entry().is_some(),
            // These act on the selection, not the cursor.
            Command::Trash | Command::DeleteForever => explorer && self.sel.count() > 0,
            Command::Layout
            | Command::Filter
            | Command::SortNext
            | Command::SortReverse
            | Command::SelectAll
            | Command::ShowHidden
            | Command::Pin
            | Command::Reload => explorer,
            Command::Shape => self.mode == ViewMode::Grid,
            Command::ZoomIn
            | Command::ZoomOut
            | Command::Fit
            | Command::Actual
            | Command::Frame => self.mode == ViewMode::Viewer,
            Command::Commands
            | Command::Help
            | Command::Debug
            | Command::Fullscreen
            | Command::Quit
            | Command::CopyPath
            | Command::CopyImage => true,
        }
    }

    /// Do what `command` says, if it can be done now.
    pub(super) fn run_command(&mut self, command: Command) {
        // A history step is one step a press whether or not there is one, or a
        // held arrow keeps repeating as a plain arrow once Alt is let go.
        if matches!(command, Command::Back | Command::Forward) {
            self.stop_repeat();
        }
        if !self.command_enabled(command) {
            return;
        }
        match command {
            Command::Commands => self.open_menu(Anchor::Centre),
            Command::Help => self.show_help = !self.show_help,
            Command::Debug => self.show_debug = !self.show_debug,
            Command::Fullscreen => self.toggle_fullscreen(),
            Command::Quit => self.closed = true,
            Command::CopyPath => self.copy_path(),
            Command::CopyImage => self.copy_image(),
            Command::Layout => self.toggle_layout(),
            Command::Shape => self.cycle_shape(),
            Command::Inspector => self.toggle_inspector(),
            Command::Filter => self.focus_filter(),
            Command::SortNext => self.set_order(self.order.next_key()),
            Command::SortReverse => self.set_order(self.order.reversed()),
            Command::SelectAll => self.sel.select_all(),
            Command::ShowHidden => self.toggle_hidden(),
            Command::Pin => self.toggle_pin(),
            Command::Reload => self.reload(),
            Command::Back => self.press_control(Control::Back),
            Command::Forward => self.press_control(Control::Forward),
            Command::Up => self.go_parent(),
            Command::Open => self.activate_lead(),
            Command::Rename => self.start_rename(),
            Command::Trash => self.trash_selection(),
            Command::DeleteForever => self.confirm_delete_selection(),
            Command::ZoomIn => self.zoom(ZOOM_STEP, false),
            Command::ZoomOut => self.zoom(1.0 / ZOOM_STEP, false),
            Command::Fit => self.view_fit(),
            Command::Actual => self.view_actual(),
            Command::Frame => self.toggle_viewer_frame(),
        }
        self.dirty = true;
    }

    // ---- the command list ------------------------------------------------

    /// Open the command list at `anchor`, offering what holds where the
    /// keyboard is. Opened on an entry (a right-click on a row), the commands
    /// about that entry come first, as a context menu's do.
    pub(super) fn open_menu(&mut self, anchor: Anchor) {
        if self.prompt.is_some() {
            return;
        }
        self.stop_repeat();
        let here = self.key_context();
        let mut items: Vec<Command> = keymap::commands_for(here)
            .into_iter()
            .filter(|c| *c != Command::Commands)
            .collect();
        if matches!(anchor, Anchor::At(..)) && here.entries && !here.viewer {
            items.sort_by_key(|c| !c.is_about_entries());
        }
        self.menu = Some(Menu::new(items, anchor));
        self.dirty = true;
    }

    pub(super) fn close_menu(&mut self) {
        self.menu = None;
        self.dirty = true;
    }

    /// Run the command the list's cursor is on, closing the list first, so
    /// the command acts on the window as it is under it.
    fn run_chosen(&mut self) {
        let chosen = self.menu.as_ref().and_then(Menu::chosen);
        self.close_menu();
        if let Some(command) = chosen {
            self.run_command(command);
        }
    }

    /// The keys, while the list is up: it takes all of them, as the prompt
    /// does. The arrows move, typing narrows, Enter runs, Escape closes.
    pub(super) fn menu_key(&mut self, action: KeyAction) {
        let ctrl = self.xkb.ctrl_active();
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        match action {
            KeyAction::Escape | KeyAction::Menu => return self.close_menu(),
            KeyAction::Enter => return self.run_chosen(),
            KeyAction::Up => menu.move_cursor(-1),
            KeyAction::Down => menu.move_cursor(1),
            KeyAction::PageUp => menu.move_cursor(-10),
            KeyAction::PageDown => menu.move_cursor(10),
            KeyAction::Left => menu.edit(TextField::left),
            KeyAction::Right => menu.edit(TextField::right),
            KeyAction::Home => menu.edit(TextField::home),
            KeyAction::End => menu.edit(TextField::end),
            KeyAction::Back => menu.edit(TextField::backspace),
            KeyAction::Delete => menu.edit(TextField::delete),
            KeyAction::Char(c) if !ctrl => menu.edit(|field| field.insert(c)),
            _ => return,
        }
        self.dirty = true;
    }

    /// Where the list and its rows are, if it is up.
    pub(super) fn menu_layout(&self) -> Option<MenuLayout> {
        let menu = self.menu.as_ref()?;
        let row_h = self.fonts.metrics(self.font_px(FONT_STATUS)).line_height + self.scale().px(8);
        let window = Rect {
            x: 0,
            y: 0,
            w: self.width as i32,
            h: self.height as i32,
        };
        Some(MenuLayout::new(
            self.scale(),
            window,
            menu.anchor,
            menu.shown().len(),
            menu.cursor(),
            row_h,
        ))
    }

    /// A left press while the list is up: a row runs its command, anywhere
    /// else closes the list. Either way the press goes no further.
    pub(super) fn menu_click(&mut self, px: f32, py: f32) {
        let Some(layout) = self.menu_layout() else {
            return;
        };
        match layout.row_at(px, py) {
            Some(row) => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.point(row);
                }
                self.run_chosen();
            }
            None if crate::explorer::contains(layout.panel, px, py) => {}
            None => self.close_menu(),
        }
    }
}
