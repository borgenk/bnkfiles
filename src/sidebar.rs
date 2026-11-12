//! The sidebar's sections: which rows it shows, in what order, and which
//! sections are folded shut.
//!
//! ```text
//!   ▾ PLACES        header row: a click or Enter folds it
//!     Home
//!     Filesystem
//!   ▾ DEVICES
//!     Data          ▬▬▬▬▬▭▭  how full, once the worker has asked
//!   ▸ PINNED        folded: its places are not rows at all
//!   ▾ COLLECTIONS
//!     refs
//! ```
//!
//! Pure: the places come from [`crate::places`], the fold state from the
//! settings, and the rows are a function of the two. A section with no places
//! has no header either, so a machine with nothing pinned shows no "Pinned".

use crate::places::{Place, PlaceKind};

/// One sidebar section, in the order they are shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    Places,
    Devices,
    Pinned,
    Collections,
}

impl Section {
    pub const ALL: [Section; 4] = [
        Section::Places,
        Section::Devices,
        Section::Pinned,
        Section::Collections,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::Places => "Places",
            Section::Devices => "Devices",
            Section::Pinned => "Pinned",
            Section::Collections => "Collections",
        }
    }

    /// The section's name in the settings file.
    pub fn key(self) -> &'static str {
        match self {
            Section::Places => "places",
            Section::Devices => "devices",
            Section::Pinned => "pinned",
            Section::Collections => "collections",
        }
    }

    pub fn from_key(key: &str) -> Option<Section> {
        Section::ALL.into_iter().find(|s| s.key() == key)
    }

    /// The section a place belongs in.
    pub fn of(kind: PlaceKind) -> Section {
        match kind {
            PlaceKind::Home | PlaceKind::Root => Section::Places,
            PlaceKind::Mount => Section::Devices,
            PlaceKind::Pinned => Section::Pinned,
            PlaceKind::Collection => Section::Collections,
        }
    }
}

/// One row of the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// A section's header, and whether it is folded.
    Header(Section, bool),
    /// A place, by its index in the places list.
    Place(usize),
}

/// The sidebar's rows: each section that has places, as a header followed by
/// its places unless it is folded. `folded` lists the sections folded shut.
pub fn rows(places: &[Place], folded: &[Section]) -> Vec<Row> {
    let mut out = Vec::new();
    for section in Section::ALL {
        let mut members = places
            .iter()
            .enumerate()
            .filter(|(_, p)| Section::of(p.kind) == section)
            .map(|(i, _)| Row::Place(i))
            .peekable();
        if members.peek().is_none() {
            continue;
        }
        let shut = folded.contains(&section);
        out.push(Row::Header(section, shut));
        if !shut {
            out.extend(members);
        }
    }
    out
}

/// Fold `section` if it is open, open it if it is folded.
pub fn toggle(folded: &mut Vec<Section>, section: Section) {
    match folded.iter().position(|s| *s == section) {
        Some(at) => {
            folded.remove(at);
        }
        None => folded.push(section),
    }
}

/// The row to put the keyboard on after a fold changed the rows: the same
/// row's new position, or the header of the section it was folded into.
pub fn follow(before: &[Row], at: usize, after: &[Row]) -> usize {
    let Some(&was) = before.get(at) else {
        return 0;
    };
    // A header is the same row folded or not.
    let same = |r: &Row| match (*r, was) {
        (Row::Header(a, _), Row::Header(b, _)) => a == b,
        (a, b) => a == b,
    };
    if let Some(now) = after.iter().position(same) {
        return now;
    }
    // Gone: a place inside a section just folded. Its section's header stays.
    let section = before[..=at].iter().rev().find_map(|r| match r {
        Row::Header(s, _) => Some(*s),
        Row::Place(_) => None,
    });
    after
        .iter()
        .position(|r| matches!(r, Row::Header(s, _) if Some(*s) == section))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::sidebar::*;

    fn place(name: &str, kind: PlaceKind) -> Place {
        Place {
            name: name.to_string(),
            path: PathBuf::from(format!("/{name}")),
            kind,
        }
    }

    fn sample() -> Vec<Place> {
        vec![
            place("home", PlaceKind::Home),
            place("root", PlaceKind::Root),
            place("data", PlaceKind::Mount),
            place("pics", PlaceKind::Pinned),
            place("refs", PlaceKind::Collection),
        ]
    }

    #[test]
    fn each_section_heads_its_places_in_order() {
        assert_eq!(
            rows(&sample(), &[]),
            [
                Row::Header(Section::Places, false),
                Row::Place(0),
                Row::Place(1),
                Row::Header(Section::Devices, false),
                Row::Place(2),
                Row::Header(Section::Pinned, false),
                Row::Place(3),
                Row::Header(Section::Collections, false),
                Row::Place(4),
            ]
        );
    }

    #[test]
    fn a_section_with_nothing_in_it_is_not_shown() {
        let places = vec![
            place("home", PlaceKind::Home),
            place("root", PlaceKind::Root),
        ];
        assert_eq!(
            rows(&places, &[Section::Pinned]),
            [
                Row::Header(Section::Places, false),
                Row::Place(0),
                Row::Place(1)
            ]
        );
    }

    #[test]
    fn a_folded_section_keeps_its_header_and_hides_its_places() {
        let r = rows(&sample(), &[Section::Places, Section::Pinned]);
        assert_eq!(
            r,
            [
                Row::Header(Section::Places, true),
                Row::Header(Section::Devices, false),
                Row::Place(2),
                Row::Header(Section::Pinned, true),
                Row::Header(Section::Collections, false),
                Row::Place(4),
            ]
        );
    }

    #[test]
    fn toggling_twice_is_where_it_started() {
        let mut folded = vec![Section::Devices];
        toggle(&mut folded, Section::Pinned);
        assert_eq!(folded, [Section::Devices, Section::Pinned]);
        toggle(&mut folded, Section::Devices);
        toggle(&mut folded, Section::Pinned);
        assert!(folded.is_empty());
    }

    #[test]
    fn the_keyboard_follows_its_row_across_a_fold() {
        let places = sample();
        let open = rows(&places, &[]);
        let shut = rows(&places, &[Section::Places]);
        // On the data mount, Places folding above it: it moves up two rows.
        assert_eq!(follow(&open, 4, &shut), 2);
        // On Home, folded away: onto the Places header.
        assert_eq!(follow(&open, 1, &shut), 0);
        // On the Places header itself: it stays the header, now folded.
        assert_eq!(follow(&open, 0, &shut), 0);
        // A header further down, folding itself: it stays put.
        let pinned_shut = rows(&places, &[Section::Pinned]);
        assert_eq!(follow(&open, 5, &pinned_shut), 5);
        // And the collection below it moves up one.
        assert_eq!(follow(&open, 8, &pinned_shut), 7);
        // Nowhere sensible: the top.
        assert_eq!(follow(&open, 99, &shut), 0);
    }

    #[test]
    fn section_keys_read_back() {
        for s in Section::ALL {
            assert_eq!(Section::from_key(s.key()), Some(s));
        }
        assert_eq!(Section::from_key("bogus"), None);
    }
}
