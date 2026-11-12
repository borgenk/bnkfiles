//! The filter field: the text typed into the header, and which names it lets
//! through.
//!
//! A filter narrows the rows of the folder already read. It is a projection
//! of the listing, like hiding dot-files, so typing re-reads nothing: each
//! keystroke is one pass over the names in memory. It matches names only, in
//! the open folder only; walking the tree below is not what it does.
//!
//! The text is a [`TextField`], the same model the rename prompt edits, so
//! the caret, the grapheme-wise Backspace and the refused characters behave
//! the same in both. A name cannot hold `/`, so a filter that refuses it
//! loses nothing.

use crate::prompt::TextField;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Filter {
    field: TextField,
    /// The text, case-folded once per edit rather than once per name.
    folded: String,
}

impl Filter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn field(&self) -> &TextField {
        &self.field
    }

    /// Whether the filter narrows anything: an empty one lets every name by.
    pub fn is_active(&self) -> bool {
        !self.field.text().is_empty()
    }

    /// Edit the text with `edit`, saying whether that changed what matches.
    /// A caret move changes nothing, and the rows need not be projected again.
    pub fn edit(&mut self, edit: impl FnOnce(&mut TextField)) -> bool {
        edit(&mut self.field);
        let folded = fold(self.field.text());
        let changed = folded != self.folded;
        self.folded = folded;
        changed
    }

    /// Empty the field. Says whether there was anything to empty.
    pub fn clear(&mut self) -> bool {
        let was_active = self.is_active();
        *self = Self::new();
        was_active
    }

    /// Whether `name` contains the text, ignoring case.
    pub fn matches(&self, name: &str) -> bool {
        if self.folded.is_empty() {
            return true;
        }
        // The common case, a plain-ASCII name, compares in place: a 100k-entry
        // folder is filtered on every keystroke, and folding each name into a
        // new string would allocate as many times.
        if name.is_ascii() && self.folded.is_ascii() {
            let needle = self.folded.as_bytes();
            return name
                .as_bytes()
                .windows(needle.len())
                .any(|w| w.eq_ignore_ascii_case(needle));
        }
        fold(name).contains(&self.folded)
    }
}

fn fold(text: &str) -> String {
    text.to_lowercase()
}

/// The part of `text` a field `width` wide shows, as a byte range, so that
/// the caret at `cursor` is in view. Text draws unclipped, so the field cuts
/// it to this itself.
///
/// The text shows from its start while it fits up to the caret; past that it
/// scrolls so the caret sits at the right edge, which is where typing happens.
/// Whatever follows the caret shows for as long as it fits. `measure` is the
/// width of a piece of text in the field's font.
pub fn visible(
    text: &str,
    cursor: usize,
    width: i32,
    measure: &dyn Fn(&str) -> i32,
) -> std::ops::Range<usize> {
    let cursor = cursor.min(text.len());
    let fits = |range: std::ops::Range<usize>| measure(&text[range]) <= width;
    let bounds = || text.char_indices().map(|(i, _)| i).chain([text.len()]);
    // The first boundary from which the text up to the caret fits.
    let start = bounds()
        .filter(|&i| i <= cursor)
        .find(|&i| fits(i..cursor))
        .unwrap_or(cursor);
    // And the last one after the caret that still fits from there.
    let end = bounds()
        .filter(|&i| i >= cursor)
        .take_while(|&i| fits(start..i))
        .last()
        .unwrap_or(cursor);
    start..end
}

#[cfg(test)]
mod tests {
    use crate::filter::*;

    fn typed(text: &str) -> Filter {
        let mut f = Filter::new();
        for c in text.chars() {
            f.edit(|field| field.insert(c));
        }
        f
    }

    #[test]
    fn an_empty_filter_lets_everything_by() {
        let f = Filter::new();
        assert!(!f.is_active());
        assert!(f.matches("anything.jpg"));
        assert!(f.matches(""));
    }

    #[test]
    fn a_name_matches_anywhere_in_it_and_in_any_case() {
        let f = typed("Beach");
        assert!(f.matches("beach.jpg"));
        assert!(f.matches("2024-BEACH-day.png"));
        assert!(f.matches("the beach"));
        assert!(!f.matches("bech.jpg"));
        assert!(!f.matches("Bea"), "shorter than the text");
    }

    #[test]
    fn names_outside_ascii_fold_too() {
        let f = typed("ÅR");
        assert!(f.matches("Sommerår.jpg"));
        assert!(!f.matches("aar.jpg"));
        // An ASCII filter still finds itself inside a name that is not.
        let f = typed("photo");
        assert!(f.matches("Ærø PHOTO.jpg"));
    }

    #[test]
    fn an_edit_says_whether_the_matches_can_have_changed() {
        let mut f = typed("ab");
        assert!(!f.edit(TextField::left), "a caret move filters nothing new");
        assert!(!f.edit(TextField::home));
        assert!(f.edit(TextField::delete), "deletes the a");
        assert_eq!(f.field().text(), "b");
        assert!(!f.edit(|field| field.insert('/')), "a name holds no slash");
        assert!(f.clear());
        assert!(!f.clear(), "nothing left to clear");
        assert_eq!(f, Filter::new());
    }

    #[test]
    fn the_field_scrolls_to_keep_the_caret_in_view() {
        // Ten pixels a character.
        let measure = |t: &str| t.chars().count() as i32 * 10;
        assert_eq!(visible("abc", 3, 100, &measure), 0..3, "fits: all of it");
        // Twelve characters in a field ten wide: the caret at the end shows
        // the last ten.
        let text = "abcdefghijkl";
        assert_eq!(visible(text, 12, 100, &measure), 2..12);
        // The caret back near the start shows the start, cut at the edge.
        assert_eq!(visible(text, 4, 100, &measure), 0..10);
        // On multi-byte characters both ends land on boundaries.
        let text = "ååååååååååå";
        let range = visible(text, text.len(), 100, &measure);
        assert!(text.is_char_boundary(range.start));
        assert_eq!(text[range].chars().count(), 10);
        assert_eq!(visible("", 0, 100, &measure), 0..0);
    }
}
