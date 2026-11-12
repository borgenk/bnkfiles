//! What a key press means: the binding table, and nothing else.
//!
//! Two layers. [`action`] names the key that was pressed (an arrow, Enter, a
//! typed character). [`BINDINGS`] says which of those, with which modifiers,
//! is a [`Command`]. The key handler runs commands through [`command`] and
//! the command list shows the same table, so a key and the menu entry for it
//! cannot drift apart: there is one place either is written down.
//!
//! [`crate::platform::xkb`] answers what the machine knows about a key (the text it
//! types, which modifiers are held). Turning that into an action lives here, where
//! you would look for it to rebind something.
//!
//! Named keys (the arrows, Enter, Escape) dispatch on the evdev keycode, because
//! their meaning does not move with the layout: the Left arrow is the Left arrow
//! on every keyboard. Everything else is the character the layout types, so `q`
//! quits on QWERTY and on Dvorak alike.

use crate::platform::xkb::{keycode, Xkb};

/// The menu key (evdev `KEY_COMPOSE`), which `xkb::keycode` does not name.
pub const MENU: u32 = 127;

/// Something the app can be told to do, from a key or from the command list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Commands,
    Help,
    Debug,
    Fullscreen,
    Quit,
    CopyPath,
    CopyImage,
    Layout,
    Shape,
    Inspector,
    Filter,
    SortNext,
    SortReverse,
    SelectAll,
    ShowHidden,
    Pin,
    Reload,
    Back,
    Forward,
    Up,
    Open,
    Rename,
    Trash,
    DeleteForever,
    ZoomIn,
    ZoomOut,
    Fit,
    Actual,
    Frame,
}

impl Command {
    /// What the command list calls it.
    pub fn title(self) -> &'static str {
        match self {
            Command::Commands => "Commands",
            Command::Help => "Keys",
            Command::Debug => "Debug overlay",
            Command::Fullscreen => "Fullscreen",
            Command::Quit => "Quit",
            Command::CopyPath => "Copy path",
            Command::CopyImage => "Copy image",
            Command::Layout => "List or thumbnails",
            Command::Shape => "Thumbnail shape: auto, square or masonry",
            Command::Inspector => "Inspector",
            Command::Filter => "Filter by name",
            Command::SortNext => "Sort by the next column",
            Command::SortReverse => "Sort the other way",
            Command::SelectAll => "Select all",
            Command::ShowHidden => "Show hidden entries",
            Command::Pin => "Pin this folder",
            Command::Reload => "Read the folder again",
            Command::Back => "Back",
            Command::Forward => "Forward",
            Command::Up => "Up to the parent folder",
            Command::Open => "Open",
            Command::Rename => "Rename",
            Command::Trash => "Move to the trash",
            Command::DeleteForever => "Delete for good",
            Command::ZoomIn => "Zoom in",
            Command::ZoomOut => "Zoom out",
            Command::Fit => "Fit",
            Command::Actual => "Actual size",
            Command::Frame => "Bare or framed",
        }
    }

    /// The command's first key, as the command list shows it.
    pub fn keys(self) -> String {
        BINDINGS
            .iter()
            .find(|b| b.command == self)
            .map(|b| b.chord.label())
            .unwrap_or_default()
    }

    /// Whether the command acts on the entries under the cursor or selected,
    /// which a context menu opened on one lists first.
    pub fn is_about_entries(self) -> bool {
        matches!(
            self,
            Command::Open | Command::Rename | Command::Trash | Command::DeleteForever
        )
    }
}

/// Where a binding holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Anywhere,
    /// The explorer, whichever pane has the keyboard.
    Explorer,
    /// The explorer's entries: what the cursor is on is what it acts on.
    Entries,
    Viewer,
}

/// Where the keyboard is, for deciding which bindings hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context {
    pub viewer: bool,
    /// In the explorer with the keyboard on the entries.
    pub entries: bool,
}

impl Scope {
    pub fn holds(self, here: Context) -> bool {
        match self {
            Scope::Anywhere => true,
            Scope::Explorer => !here.viewer,
            Scope::Entries => !here.viewer && here.entries,
            Scope::Viewer => here.viewer,
        }
    }
}

/// The modifiers held with a key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

/// A key and the modifiers it is pressed with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    /// A named key, or a typed character (letters lower-case).
    pub key: KeyAction,
    pub mods: Mods,
}

impl Chord {
    const fn key(key: KeyAction) -> Self {
        Self {
            key,
            mods: Mods {
                ctrl: false,
                shift: false,
                alt: false,
            },
        }
    }

    const fn ctrl(key: KeyAction) -> Self {
        Self {
            key,
            mods: Mods {
                ctrl: true,
                shift: false,
                alt: false,
            },
        }
    }

    const fn shift(key: KeyAction) -> Self {
        Self {
            key,
            mods: Mods {
                ctrl: false,
                shift: true,
                alt: false,
            },
        }
    }

    const fn alt(key: KeyAction) -> Self {
        Self {
            key,
            mods: Mods {
                ctrl: false,
                shift: false,
                alt: true,
            },
        }
    }

    /// Whether `pressed` with `mods` is this chord. A letter matches in either
    /// case, so Caps Lock does not change what a key does, and Shift and Alt
    /// are then read from the modifiers. Any other character is whatever the
    /// layout typed, with Shift or AltGr as that layout needs (`?` is Shift+/
    /// on one layout and AltGr+W on another), so only Ctrl is compared for it.
    fn matches(&self, pressed: KeyAction, mods: Mods) -> bool {
        let ctrl = mods.ctrl == self.mods.ctrl;
        let shift_alt = mods.shift == self.mods.shift && mods.alt == self.mods.alt;
        match (self.key, pressed) {
            (KeyAction::Char(want), KeyAction::Char(got)) if !want.is_alphabetic() => {
                got == want && ctrl
            }
            (KeyAction::Char(want), KeyAction::Char(got)) => {
                got.to_lowercase().eq(want.to_lowercase()) && ctrl && shift_alt
            }
            (want, got) => want == got && ctrl && shift_alt,
        }
    }

    /// The key as it arrives when this chord is pressed: a letter with Shift
    /// comes in upper-case. For the tests, which press each chord.
    #[cfg(test)]
    fn pressed(&self) -> KeyAction {
        match self.key {
            KeyAction::Char(c) if self.mods.shift => KeyAction::Char(c.to_ascii_uppercase()),
            key => key,
        }
    }

    /// How the chord is written for a person: `Ctrl+A`, `Shift+Delete`, `/`.
    pub fn label(&self) -> String {
        let key = match self.key {
            KeyAction::Char(c) => c.to_uppercase().collect(),
            KeyAction::Left => "←".to_string(),
            KeyAction::Right => "→".to_string(),
            KeyAction::Up => "↑".to_string(),
            KeyAction::Down => "↓".to_string(),
            KeyAction::Enter => "Enter".to_string(),
            KeyAction::Escape => "Esc".to_string(),
            KeyAction::Home => "Home".to_string(),
            KeyAction::End => "End".to_string(),
            KeyAction::PageUp => "PgUp".to_string(),
            KeyAction::PageDown => "PgDn".to_string(),
            KeyAction::Back => "Backspace".to_string(),
            KeyAction::Tab => "Tab".to_string(),
            KeyAction::Rename => "F2".to_string(),
            KeyAction::Reload => "F5".to_string(),
            KeyAction::Delete => "Delete".to_string(),
            KeyAction::Menu => "Menu".to_string(),
            KeyAction::None => String::new(),
        };
        let mut out = String::new();
        for (held, name) in [
            (self.mods.ctrl, "Ctrl+"),
            (self.mods.alt, "Alt+"),
            (self.mods.shift, "Shift+"),
        ] {
            if held {
                out.push_str(name);
            }
        }
        out.push_str(&key);
        out
    }
}

/// One line of the binding table.
#[derive(Clone, Copy, Debug)]
pub struct Binding {
    pub chord: Chord,
    pub command: Command,
    pub scope: Scope,
}

const fn bind(chord: Chord, command: Command, scope: Scope) -> Binding {
    Binding {
        chord,
        command,
        scope,
    }
}

/// Every key that is a command, in the order the command list shows them. A
/// command's first line is the key the list names for it.
///
/// Not here: the keys that move (arrows, Home, End, the pages), and Escape,
/// which means "the nearest thing to undo" and so depends on what is open.
pub const BINDINGS: &[Binding] = {
    use Command as C;
    use KeyAction as K;
    use Scope as S;
    &[
        bind(Chord::key(K::Enter), C::Open, S::Entries),
        bind(Chord::key(K::Back), C::Up, S::Entries),
        bind(Chord::alt(K::Left), C::Back, S::Anywhere),
        // Explorer only: nothing is ahead of a picture, and in the viewer
        // Alt+Right means Right, the next picture.
        bind(Chord::alt(K::Right), C::Forward, S::Explorer),
        bind(Chord::key(K::Char('/')), C::Filter, S::Explorer),
        bind(Chord::key(K::Char('s')), C::SortNext, S::Explorer),
        bind(Chord::shift(K::Char('s')), C::SortReverse, S::Explorer),
        bind(Chord::key(K::Char('v')), C::Layout, S::Explorer),
        bind(Chord::key(K::Char('g')), C::Shape, S::Explorer),
        bind(Chord::key(K::Reload), C::Reload, S::Explorer),
        bind(Chord::ctrl(K::Char('r')), C::Reload, S::Explorer),
        bind(Chord::key(K::Char('i')), C::Inspector, S::Anywhere),
        bind(Chord::ctrl(K::Char('a')), C::SelectAll, S::Explorer),
        bind(Chord::ctrl(K::Char('h')), C::ShowHidden, S::Explorer),
        bind(Chord::ctrl(K::Char('d')), C::Pin, S::Explorer),
        bind(Chord::key(K::Rename), C::Rename, S::Entries),
        bind(Chord::key(K::Delete), C::Trash, S::Entries),
        bind(Chord::shift(K::Delete), C::DeleteForever, S::Entries),
        bind(Chord::key(K::Char('+')), C::ZoomIn, S::Viewer),
        bind(Chord::key(K::Char('=')), C::ZoomIn, S::Viewer),
        bind(Chord::key(K::Char('-')), C::ZoomOut, S::Viewer),
        bind(Chord::key(K::Char('_')), C::ZoomOut, S::Viewer),
        bind(Chord::key(K::Char('0')), C::Fit, S::Viewer),
        // Ctrl with the zoom keys, as most viewers take them.
        bind(Chord::ctrl(K::Char('+')), C::ZoomIn, S::Viewer),
        bind(Chord::ctrl(K::Char('=')), C::ZoomIn, S::Viewer),
        bind(Chord::ctrl(K::Char('-')), C::ZoomOut, S::Viewer),
        bind(Chord::ctrl(K::Char('0')), C::Fit, S::Viewer),
        bind(Chord::key(K::Char('1')), C::Actual, S::Viewer),
        bind(Chord::key(K::Char('b')), C::Frame, S::Viewer),
        bind(Chord::key(K::Char('c')), C::CopyPath, S::Anywhere),
        bind(Chord::key(K::Char('y')), C::CopyImage, S::Anywhere),
        bind(Chord::key(K::Char('f')), C::Fullscreen, S::Anywhere),
        bind(Chord::ctrl(K::Char('p')), C::Commands, S::Anywhere),
        bind(Chord::key(K::Menu), C::Commands, S::Anywhere),
        bind(Chord::key(K::Char('h')), C::Help, S::Anywhere),
        bind(Chord::key(K::Char('?')), C::Help, S::Anywhere),
        bind(Chord::key(K::Char('d')), C::Debug, S::Anywhere),
        bind(Chord::key(K::Char('q')), C::Quit, S::Anywhere),
    ]
};

/// The command `pressed` stands for with `mods` held, where the keyboard is.
pub fn command(pressed: KeyAction, mods: Mods, here: Context) -> Option<Command> {
    BINDINGS
        .iter()
        .find(|b| b.scope.holds(here) && b.chord.matches(pressed, mods))
        .map(|b| b.command)
}

/// The commands that mean something `here`, each once, in the table's order.
pub fn commands_for(here: Context) -> Vec<Command> {
    let mut out: Vec<Command> = Vec::new();
    for b in BINDINGS {
        if b.scope.holds(here) && !out.contains(&b.command) {
            out.push(b.command);
        }
    }
    out
}

/// What a key press should do. The view decides what it means (Left is the
/// previous image in the viewer and the cell to the left in the grid); this is
/// only the key's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Char(char),
    Left,
    Right,
    Up,
    Down,
    Enter,
    Escape,
    /// Jump to the first or last entry.
    Home,
    End,
    PageUp,
    PageDown,
    /// Up to the parent directory.
    Back,
    /// Move the focus between the entries, the sidebar, and the breadcrumb.
    Tab,
    /// Rename what the cursor is on.
    Rename,
    /// Read the folder again.
    Reload,
    /// Send the selection to the trash, or (with Shift) remove it for good.
    Delete,
    /// The keyboard's menu key, which opens the command list.
    Menu,
    None,
}

impl KeyAction {
    /// Whether holding the key should repeat this action. Anything that *moves*
    /// repeats — an arrow held down walks a folder, and a held Backspace climbs out
    /// of a deep tree — while a command does not, so holding `f` will not strobe
    /// the window in and out of fullscreen.
    ///
    /// Home and End are deliberately absent: they are idempotent, so repeating them
    /// only burns wake-ups arriving at the same row.
    pub fn repeats(self) -> bool {
        matches!(
            self,
            KeyAction::Left
                | KeyAction::Right
                | KeyAction::Up
                | KeyAction::Down
                | KeyAction::PageUp
                | KeyAction::PageDown
                | KeyAction::Back
        )
    }
}

/// The action for a Wayland (evdev) keycode, read against `xkb`'s current layout
/// and modifier state.
///
/// Modifiers are deliberately not folded in here. Whether Shift extends a selection
/// or Ctrl toggles one is a question about the view the key landed in, so the view
/// asks `xkb` at the point of use and this stays a statement about the key alone.
pub fn action(xkb: &Xkb, code: u32) -> KeyAction {
    match code {
        keycode::LEFT => KeyAction::Left,
        keycode::RIGHT => KeyAction::Right,
        keycode::UP => KeyAction::Up,
        keycode::DOWN => KeyAction::Down,
        keycode::ENTER | keycode::KP_ENTER => KeyAction::Enter,
        keycode::ESC => KeyAction::Escape,
        keycode::HOME => KeyAction::Home,
        keycode::END => KeyAction::End,
        keycode::PAGEUP => KeyAction::PageUp,
        keycode::PAGEDOWN => KeyAction::PageDown,
        keycode::BACKSPACE => KeyAction::Back,
        keycode::TAB => KeyAction::Tab,
        keycode::DELETE => KeyAction::Delete,
        keycode::F2 => KeyAction::Rename,
        keycode::F5 => KeyAction::Reload,
        MENU => KeyAction::Menu,
        // Everything else is the character the layout types, composed if the
        // layout composes it.
        _ => xkb.key_text(code).map_or(KeyAction::None, KeyAction::Char),
    }
}

#[cfg(test)]
mod tests {
    use crate::keymap::*;
    use crate::platform::xkb::keycode;
    use crate::platform::xkb::Xkb;

    /// The key that produces `what`, for the actions that are a named key rather
    /// than a typed character.
    ///
    /// The match has no wildcard arm on purpose. Adding a variant to [`KeyAction`]
    /// without giving it a key stops this compiling, which is the check that every
    /// action the app can name stays reachable from the keyboard.
    fn key_for(what: KeyAction) -> Option<u32> {
        match what {
            KeyAction::Left => Some(keycode::LEFT),
            KeyAction::Right => Some(keycode::RIGHT),
            KeyAction::Up => Some(keycode::UP),
            KeyAction::Down => Some(keycode::DOWN),
            KeyAction::Enter => Some(keycode::ENTER),
            KeyAction::Escape => Some(keycode::ESC),
            KeyAction::Home => Some(keycode::HOME),
            KeyAction::End => Some(keycode::END),
            KeyAction::PageUp => Some(keycode::PAGEUP),
            KeyAction::PageDown => Some(keycode::PAGEDOWN),
            KeyAction::Back => Some(keycode::BACKSPACE),
            KeyAction::Tab => Some(keycode::TAB),
            KeyAction::Rename => Some(keycode::F2),
            KeyAction::Reload => Some(keycode::F5),
            KeyAction::Delete => Some(keycode::DELETE),
            KeyAction::Menu => Some(MENU),
            // Not named keys: one is whatever the layout types, the other is the
            // absence of an action.
            KeyAction::Char(_) | KeyAction::None => None,
        }
    }

    /// Every named action, so the coverage check below has a list to walk.
    const NAMED: &[KeyAction] = &[
        KeyAction::Left,
        KeyAction::Right,
        KeyAction::Up,
        KeyAction::Down,
        KeyAction::Enter,
        KeyAction::Escape,
        KeyAction::Home,
        KeyAction::End,
        KeyAction::PageUp,
        KeyAction::PageDown,
        KeyAction::Back,
        KeyAction::Tab,
        KeyAction::Rename,
        KeyAction::Reload,
        KeyAction::Delete,
        KeyAction::Menu,
    ];

    #[test]
    fn every_named_action_has_a_key_that_produces_it() {
        // The explorer's promise is that nothing needs the pointer. This is the
        // half of it a unit test can hold: every action the keymap can report is
        // one some key actually resolves to, with no gaps and no duplicates.
        let xkb = Xkb::new().expect("xkb");
        let mut seen: Vec<u32> = Vec::new();
        for &want in NAMED {
            let code = key_for(want).expect("a named action must name a key");
            assert_eq!(
                action(&xkb, code),
                want,
                "keycode {code} does not produce {want:?}"
            );
            assert!(
                !seen.contains(&code),
                "keycode {code} is bound twice, so one action is unreachable"
            );
            seen.push(code);
        }
        // The keypad's Enter is the one deliberate second binding.
        assert_eq!(action(&xkb, keycode::KP_ENTER), KeyAction::Enter);
    }

    #[test]
    fn only_navigation_repeats() {
        // Holding an arrow walks the folder; holding a command must not fire it
        // over and over (F would strobe the window in and out of fullscreen).
        assert!(KeyAction::Left.repeats());
        assert!(KeyAction::Down.repeats());
        assert!(KeyAction::PageDown.repeats());
        assert!(KeyAction::Back.repeats(), "held Backspace climbs the tree");
        assert!(!KeyAction::Char('f').repeats());
        assert!(!KeyAction::Enter.repeats());
        assert!(!KeyAction::Escape.repeats());
        assert!(
            !KeyAction::Tab.repeats(),
            "focus would spin through the panes"
        );
        assert!(
            !KeyAction::Home.repeats(),
            "idempotent, so pointless to repeat"
        );
        assert!(!KeyAction::End.repeats());
        assert!(!KeyAction::None.repeats());
    }

    const EVERYWHERE: [Context; 3] = [
        Context {
            viewer: false,
            entries: true,
        },
        Context {
            viewer: false,
            entries: false,
        },
        Context {
            viewer: true,
            entries: false,
        },
    ];

    #[test]
    fn every_command_has_a_key_and_its_key_runs_it() {
        // The command list names each command's key from the same table the
        // key handler reads. Here: every command has one, and pressing that
        // key where the command holds runs that command and no other.
        let all = [
            Command::Commands,
            Command::Help,
            Command::Debug,
            Command::Fullscreen,
            Command::Quit,
            Command::CopyPath,
            Command::CopyImage,
            Command::Layout,
            Command::Shape,
            Command::Inspector,
            Command::Filter,
            Command::SortNext,
            Command::SortReverse,
            Command::SelectAll,
            Command::ShowHidden,
            Command::Pin,
            Command::Reload,
            Command::Back,
            Command::Forward,
            Command::Up,
            Command::Open,
            Command::Rename,
            Command::Trash,
            Command::DeleteForever,
            Command::ZoomIn,
            Command::ZoomOut,
            Command::Fit,
            Command::Actual,
            Command::Frame,
        ];
        // A command added to the enum without being added above stops this
        // compiling (the match has no wildcard), then fails the count.
        let number = |c: Command| match c {
            Command::Commands => 0,
            Command::Help => 1,
            Command::Debug => 2,
            Command::Fullscreen => 3,
            Command::Quit => 4,
            Command::CopyPath => 5,
            Command::CopyImage => 6,
            Command::Layout => 7,
            Command::Inspector => 8,
            Command::Filter => 9,
            Command::SortNext => 10,
            Command::SortReverse => 11,
            Command::SelectAll => 12,
            Command::ShowHidden => 13,
            Command::Pin => 14,
            Command::Back => 15,
            Command::Forward => 16,
            Command::Up => 17,
            Command::Open => 18,
            Command::Rename => 19,
            Command::Trash => 20,
            Command::DeleteForever => 21,
            Command::ZoomIn => 22,
            Command::ZoomOut => 23,
            Command::Fit => 24,
            Command::Actual => 25,
            Command::Frame => 26,
            Command::Shape => 27,
            Command::Reload => 28,
        };
        let mut numbers: Vec<usize> = all.iter().map(|c| number(*c)).collect();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(numbers.len(), 29, "every command is in the list, once");
        for want in all {
            assert!(!want.keys().is_empty(), "{want:?} has no key");
        }
        // Every line of the table, not only each command's first: pressing
        // its chord where it holds runs its command and no other.
        for b in BINDINGS {
            let here = EVERYWHERE
                .into_iter()
                .find(|c| b.scope.holds(*c))
                .expect("a scope that holds somewhere");
            assert_eq!(
                command(b.chord.pressed(), b.chord.mods, here),
                Some(b.command),
                "{} does not run {:?}",
                b.chord.label(),
                b.command
            );
        }
    }

    #[test]
    fn no_key_means_two_commands_in_one_place() {
        for (i, a) in BINDINGS.iter().enumerate() {
            for b in &BINDINGS[i + 1..] {
                // Compared the way dispatch compares, not field by field: `s`
                // and `S` without Shift are the same key to it.
                let clash = a.chord.matches(b.chord.pressed(), b.chord.mods)
                    && EVERYWHERE
                        .into_iter()
                        .any(|c| a.scope.holds(c) && b.scope.holds(c));
                assert!(
                    !clash || a.command == b.command,
                    "{} is both {:?} and {:?}",
                    a.chord.label(),
                    a.command,
                    b.command
                );
            }
        }
    }

    #[test]
    fn a_letter_ignores_caps_lock_and_shift_picks_its_own_command() {
        let entries = EVERYWHERE[0];
        let none = Mods::default();
        let shift = Mods {
            shift: true,
            ..none
        };
        assert_eq!(
            command(KeyAction::Char('s'), none, entries),
            Some(Command::SortNext)
        );
        // Caps Lock: an upper-case letter with no Shift held.
        assert_eq!(
            command(KeyAction::Char('S'), none, entries),
            Some(Command::SortNext)
        );
        assert_eq!(
            command(KeyAction::Char('S'), shift, entries),
            Some(Command::SortReverse)
        );
        // A character a layout types with Shift or AltGr is itself either way.
        let altgr = Mods { alt: true, ..none };
        assert_eq!(
            command(KeyAction::Char('/'), altgr, entries),
            Some(Command::Filter)
        );
        // But Alt with a letter is not the letter.
        assert_eq!(command(KeyAction::Char('q'), altgr, entries), None);
        assert_eq!(
            command(KeyAction::Char('?'), shift, entries),
            Some(Command::Help)
        );
        // Ctrl changes the command, and an unbound chord is nothing.
        let ctrl = Mods { ctrl: true, ..none };
        assert_eq!(
            command(KeyAction::Char('a'), ctrl, entries),
            Some(Command::SelectAll)
        );
        assert_eq!(command(KeyAction::Char('q'), ctrl, entries), None);
    }

    #[test]
    fn a_command_holds_only_where_it_means_something() {
        let [entries, sidebar, viewer] = EVERYWHERE;
        let none = Mods::default();
        // Enter opens from the entries; in the sidebar it is the sidebar's.
        assert_eq!(
            command(KeyAction::Enter, none, entries),
            Some(Command::Open)
        );
        assert_eq!(command(KeyAction::Enter, none, sidebar), None);
        assert_eq!(
            command(KeyAction::Char('0'), none, viewer),
            Some(Command::Fit)
        );
        assert_eq!(command(KeyAction::Char('0'), none, entries), None);
        // The list offers what holds, each once, without the viewer's in the
        // explorer.
        let listed = commands_for(entries);
        assert!(listed.contains(&Command::Open) && listed.contains(&Command::Quit));
        assert!(!listed.contains(&Command::Fit));
        assert_eq!(
            listed.iter().filter(|c| **c == Command::Commands).count(),
            1,
            "two keys, one entry"
        );
        assert_eq!(Command::Commands.keys(), "Ctrl+P");
        assert_eq!(Command::DeleteForever.keys(), "Shift+Delete");
        assert_eq!(Command::Back.keys(), "Alt+←");
    }
}
