//! What the window is asking the user, while it is asking it.
//!
//! Three of the file operations cannot go ahead without an answer: a rename needs
//! a name, a copy onto something that already exists needs to know which one to
//! keep, and a permanent delete needs to be sure. Each is modal — the entry list
//! stops taking keys while one is up — so they are one type rather than three
//! flags, and the model lives here, away from Wayland and the display list, where
//! its behaviour is pinned by tests.
//!
//! Two rules the answers follow, both about not destroying anything by accident:
//!
//! - A destructive choice is never the one already highlighted. Enter on a fresh
//!   confirmation cancels; deleting takes a deliberate move first.
//! - A name that could not be renamed to is never reachable. The field refuses the
//!   separator and the control characters outright, so there is no way to type a
//!   name and then be told it was not allowed.

use std::path::PathBuf;

use crate::platform::grapheme;

/// A single-line text field: a buffer, and where the cursor sits in it.
///
/// The cursor is a byte offset kept on a character boundary, so a name with an
/// accent or an emoji in it moves one *character* per arrow press rather than
/// landing inside a multi-byte sequence.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TextField {
    text: String,
    cursor: usize,
}

impl TextField {
    /// A field holding `text`, with the cursor at the end of the name's stem.
    ///
    /// Before the extension rather than after it: renaming `holiday.jpg` is nearly
    /// always about the `holiday` part, and putting the cursor there means the
    /// common case needs no arrow keys at all. A leading dot is a hidden marker
    /// and not an extension, so `.bashrc` opens with the cursor at the end.
    pub fn for_name(text: &str) -> Self {
        let cursor = match text.rfind('.') {
            Some(0) | None => text.len(),
            Some(dot) => dot,
        };
        Self {
            text: text.to_string(),
            cursor,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Type a character, ignoring the ones a file name cannot hold.
    ///
    /// The separator would make the rename a move to somewhere else, and the
    /// control characters (including the newline an Enter would otherwise insert)
    /// make names that are a misery to work with from anywhere else. Refusing them
    /// here means the field can never hold a name the rename would reject.
    pub fn insert(&mut self, c: char) {
        if c == '/' || c.is_control() {
            return;
        }
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Delete the character before the cursor: the whole grapheme cluster, so a
    /// flag or a letter with its combining mark goes in one keystroke rather than
    /// leaving half of itself behind.
    pub fn backspace(&mut self) {
        let start = grapheme::prev_boundary(&self.text, self.cursor);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Delete the character after the cursor.
    pub fn delete(&mut self) {
        let end = grapheme::next_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..end, "");
    }

    pub fn left(&mut self) {
        self.cursor = grapheme::prev_boundary(&self.text, self.cursor);
    }

    pub fn right(&mut self) {
        self.cursor = grapheme::next_boundary(&self.text, self.cursor);
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.text.len();
    }

    /// Whether the field holds something that could actually be renamed to.
    pub fn is_usable(&self) -> bool {
        !self.text.is_empty() && self.text != "." && self.text != ".."
    }
}

/// What to do about a destination that is already there. The order is the order
/// the choices are shown in, and the first is the one highlighted to begin with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConflictChoice {
    Skip,
    Overwrite,
    KeepBoth,
}

impl ConflictChoice {
    /// Skip first, because it is the one that cannot lose anything.
    pub const ALL: [ConflictChoice; 3] = [
        ConflictChoice::Skip,
        ConflictChoice::Overwrite,
        ConflictChoice::KeepBoth,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ConflictChoice::Skip => "Skip",
            ConflictChoice::Overwrite => "Overwrite",
            ConflictChoice::KeepBoth => "Keep both",
        }
    }
}

/// A question the window is waiting on an answer to.
pub enum Prompt {
    /// Renaming one entry.
    Rename { path: PathBuf, field: TextField },
    /// A job has stopped on a destination that already exists.
    Conflict {
        path: PathBuf,
        choice: usize,
        /// Whether the answer should stand for the rest of the job.
        all: bool,
    },
    /// About to delete without the trash. `yes` is false to begin with, so Enter
    /// on an unread prompt cancels rather than deletes.
    ConfirmDelete { paths: Vec<PathBuf>, yes: bool },
}

impl Prompt {
    pub fn rename(path: PathBuf, name: &str) -> Self {
        Prompt::Rename {
            path,
            field: TextField::for_name(name),
        }
    }

    pub fn conflict(path: PathBuf) -> Self {
        Prompt::Conflict {
            path,
            choice: 0,
            all: false,
        }
    }

    pub fn confirm_delete(paths: Vec<PathBuf>) -> Self {
        Prompt::ConfirmDelete { paths, yes: false }
    }

    /// Move the highlighted answer along by `delta`, clamped to the ends.
    ///
    /// Clamped rather than wrapping: a held arrow must not carry the highlight
    /// round from "Skip" to "Overwrite", and on the delete confirmation it must
    /// not wrap from "Cancel" onto "Delete".
    pub fn move_choice(&mut self, delta: i32) {
        match self {
            Prompt::Conflict { choice, .. } => {
                let last = ConflictChoice::ALL.len() as i32 - 1;
                *choice = (*choice as i32 + delta).clamp(0, last) as usize;
            }
            Prompt::ConfirmDelete { yes, .. } => {
                *yes = match delta.cmp(&0) {
                    std::cmp::Ordering::Greater => true,
                    std::cmp::Ordering::Less => false,
                    std::cmp::Ordering::Equal => *yes,
                };
            }
            Prompt::Rename { .. } => {}
        }
    }

    /// The conflict answer currently highlighted.
    pub fn conflict_choice(&self) -> Option<ConflictChoice> {
        match self {
            Prompt::Conflict { choice, .. } => ConflictChoice::ALL.get(*choice).copied(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::prompt::*;

    #[test]
    fn a_rename_field_opens_with_the_cursor_at_the_end_of_the_stem() {
        // The common case needs no arrows: the extension is already past the
        // cursor and typing lands where the name is.
        let field = TextField::for_name("holiday.jpg");
        assert_eq!(field.cursor(), "holiday".len());

        // A compound extension splits at the last dot, which is the one a rename
        // usually wants to keep.
        assert_eq!(
            TextField::for_name("archive.tar.gz").cursor(),
            "archive.tar".len()
        );
        // No extension: the end.
        assert_eq!(TextField::for_name("README").cursor(), "README".len());
        // A leading dot marks a hidden file rather than an extension.
        assert_eq!(TextField::for_name(".bashrc").cursor(), ".bashrc".len());
    }

    #[test]
    fn typing_edits_at_the_cursor() {
        let mut field = TextField::for_name("holiday.jpg");
        field.insert('s');
        assert_eq!(field.text(), "holidays.jpg");
        field.backspace();
        field.backspace();
        assert_eq!(field.text(), "holida.jpg");
        // Delete takes the character after the cursor, which is the dot.
        field.delete();
        assert_eq!(field.text(), "holidajpg");
    }

    #[test]
    fn the_field_refuses_characters_a_name_cannot_hold() {
        // Not "type it and be told no afterwards": a name that would be rejected
        // is one the field will not hold in the first place.
        let mut field = TextField::for_name("notes");
        field.insert('/');
        field.insert('\n');
        field.insert('\0');
        assert_eq!(field.text(), "notes", "nothing forbidden got in");
        field.insert('é');
        assert_eq!(field.text(), "notesé", "but ordinary text does");
    }

    #[test]
    fn the_cursor_moves_by_characters_not_bytes() {
        // A multi-byte character must move the cursor past all of it, or the next
        // edit would split it and the name would stop being valid text.
        let mut field = TextField::for_name("é");
        assert_eq!(field.cursor(), 2, "one character, two bytes");
        field.left();
        assert_eq!(field.cursor(), 0);
        field.right();
        assert_eq!(field.cursor(), 2);
        // And past the ends it simply stops.
        field.right();
        assert_eq!(field.cursor(), 2);
        field.left();
        field.left();
        assert_eq!(field.cursor(), 0);
    }

    #[test]
    fn editing_an_empty_field_does_nothing_rather_than_panicking() {
        let mut field = TextField::for_name("");
        field.backspace();
        field.delete();
        field.left();
        field.right();
        assert_eq!(field.text(), "");
        assert!(!field.is_usable(), "an empty name is not a name");
    }

    #[test]
    fn the_names_a_rename_must_not_take_are_refused() {
        assert!(TextField::for_name("photo.jpg").is_usable());
        assert!(!TextField::for_name("").is_usable());
        assert!(!TextField::for_name(".").is_usable());
        assert!(!TextField::for_name("..").is_usable());
    }

    #[test]
    fn home_and_end_go_to_the_ends() {
        let mut field = TextField::for_name("holiday.jpg");
        field.home();
        assert_eq!(field.cursor(), 0);
        field.end();
        assert_eq!(field.cursor(), "holiday.jpg".len());
    }

    #[test]
    fn a_conflict_starts_on_the_answer_that_cannot_lose_anything() {
        let mut prompt = Prompt::conflict(PathBuf::from("/tmp/a.txt"));
        assert_eq!(prompt.conflict_choice(), Some(ConflictChoice::Skip));

        prompt.move_choice(1);
        assert_eq!(prompt.conflict_choice(), Some(ConflictChoice::Overwrite));
        prompt.move_choice(1);
        assert_eq!(prompt.conflict_choice(), Some(ConflictChoice::KeepBoth));

        // Clamped at both ends: a held arrow must not carry the highlight round
        // from "Keep both" back onto "Overwrite".
        prompt.move_choice(1);
        assert_eq!(prompt.conflict_choice(), Some(ConflictChoice::KeepBoth));
        prompt.move_choice(-9);
        assert_eq!(prompt.conflict_choice(), Some(ConflictChoice::Skip));
        prompt.move_choice(-1);
        assert_eq!(prompt.conflict_choice(), Some(ConflictChoice::Skip));
    }

    #[test]
    fn editing_moves_and_deletes_whole_clusters() {
        // A flag is two scalars and "é" here is a letter plus a combining mark:
        // each is one thing on screen, so each is one keystroke.
        let mut field = TextField::for_name("a🇳🇴e\u{301}");
        field.end();
        field.backspace();
        assert_eq!(field.text(), "a🇳🇴");
        field.left();
        assert_eq!(field.cursor(), 1, "left steps over the whole flag");
        field.delete();
        assert_eq!(field.text(), "a");
        field.right();
        field.delete();
        field.home();
        field.backspace();
        assert_eq!(field.text(), "a", "nothing to delete at either end");
    }

    #[test]
    fn a_permanent_delete_is_not_the_answer_already_highlighted() {
        // Enter on a prompt nobody has read must not delete anything.
        let mut prompt = Prompt::confirm_delete(vec![PathBuf::from("/tmp/a.txt")]);
        assert!(
            matches!(prompt, Prompt::ConfirmDelete { yes: false, .. }),
            "the safe answer is the one selected to begin with"
        );

        prompt.move_choice(1);
        assert!(matches!(prompt, Prompt::ConfirmDelete { yes: true, .. }));
        prompt.move_choice(-1);
        assert!(matches!(prompt, Prompt::ConfirmDelete { yes: false, .. }));
    }
}
