//! The order a listing is in: which column it is sorted by, and which way.
//!
//! Folders always come first, whatever the column: the order a person scans a
//! folder in is "where can I go, then what is here". Within each group the
//! column decides, and the name breaks every tie, so the order is total and no
//! two sorts of the same folder can disagree.
//!
//! The scan worker sorts each batch with this and the event loop merges the
//! batches with it, so the order lives with the scan: changing it re-reads the
//! folder on the worker rather than re-sorting a large listing on the loop.

use std::cmp::Ordering;

use crate::files::{natural_cmp, FsEntry};

/// The column a listing is sorted by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
}

impl SortKey {
    pub const ALL: [SortKey; 3] = [SortKey::Name, SortKey::Size, SortKey::Modified];

    /// The column's heading, and its name in the settings file.
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "Name",
            SortKey::Size => "Size",
            SortKey::Modified => "Modified",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Modified => "modified",
        }
    }

    pub fn from_key(key: &str) -> Option<SortKey> {
        SortKey::ALL.into_iter().find(|k| k.key() == key)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Order {
    pub key: SortKey,
    pub descending: bool,
}

impl Default for Order {
    /// Names, A to Z.
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            descending: false,
        }
    }
}

impl Order {
    /// Compare two entries of one folder in this order.
    ///
    /// A folder's own size is its inode's, which says nothing about what is
    /// in it, so folders sorted by size fall back to their names.
    pub fn cmp(&self, a: &FsEntry, b: &FsEntry) -> Ordering {
        let folders_first = b.is_dir().cmp(&a.is_dir());
        if folders_first != Ordering::Equal {
            return folders_first;
        }
        let by_name = || natural_cmp(&a.name, &b.name);
        // Not turned round with the column: folders sorted by size are in
        // name order, A to Z, whichever way the files go.
        if self.key == SortKey::Size && a.is_dir() {
            return by_name();
        }
        let column = match self.key {
            SortKey::Name => by_name(),
            SortKey::Size => a.size.cmp(&b.size).then_with(by_name),
            SortKey::Modified => a.mtime_secs.cmp(&b.mtime_secs).then_with(by_name),
        };
        if self.descending {
            column.reverse()
        } else {
            column
        }
    }

    /// Sort by `key`: the other way if it is the column already sorted by,
    /// as a click on a column's heading does. A new column starts the way a
    /// person usually wants it: names A to Z, the biggest and newest first.
    pub fn by(self, key: SortKey) -> Order {
        if key == self.key {
            return Order {
                descending: !self.descending,
                ..self
            };
        }
        Order {
            key,
            descending: key != SortKey::Name,
        }
    }

    /// The next column along, for the key that steps through them.
    pub fn next_key(self) -> Order {
        let at = SortKey::ALL
            .iter()
            .position(|k| *k == self.key)
            .unwrap_or(0);
        self.by(SortKey::ALL[(at + 1) % SortKey::ALL.len()])
    }

    /// The same column the other way.
    pub fn reversed(self) -> Order {
        self.by(self.key)
    }

    /// Where a folder called `name`, now gone, sat among `entries`, which are
    /// in this order: how many of them come before it. The cursor goes there,
    /// onto whatever took its place, when the explorer climbs out of it.
    ///
    /// Folders are ordered by name in every column but the date, so the place
    /// is known in those. By date it is not: a folder that is gone has no
    /// time to be compared by, and `None` says so.
    pub fn folder_slot<'a>(
        &self,
        name: &str,
        entries: impl IntoIterator<Item = &'a FsEntry>,
    ) -> Option<usize> {
        let descending = match self.key {
            SortKey::Name => self.descending,
            SortKey::Size => false,
            SortKey::Modified => return None,
        };
        let mut before = 0;
        for entry in entries {
            if !entry.is_dir() {
                break;
            }
            let along = natural_cmp(name, &entry.name);
            let along = if descending { along.reverse() } else { along };
            if along != Ordering::Greater {
                break;
            }
            before += 1;
        }
        Some(before)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::files::FsKind;
    use crate::order::*;

    fn entry(name: &str, kind: FsKind, size: u64, mtime: i64) -> FsEntry {
        FsEntry {
            name: name.to_string(),
            path: PathBuf::from(format!("/d/{name}")),
            kind,
            size,
            mtime_secs: mtime,
            hidden: false,
        }
    }

    fn sample() -> Vec<FsEntry> {
        vec![
            entry("b.jpg", FsKind::File, 300, 10),
            entry("zdir", FsKind::Dir, 4096, 5),
            entry("a.jpg", FsKind::File, 100, 30),
            entry("img10.jpg", FsKind::File, 300, 20),
            entry("adir", FsKind::Dir, 8192, 50),
            entry("img9.jpg", FsKind::File, 200, 20),
        ]
    }

    fn sorted(order: Order) -> Vec<String> {
        let mut v = sample();
        v.sort_by(|a, b| order.cmp(a, b));
        v.into_iter().map(|e| e.name).collect()
    }

    #[test]
    fn by_name_is_the_order_the_listing_always_had() {
        assert_eq!(
            sorted(Order::default()),
            ["adir", "zdir", "a.jpg", "b.jpg", "img9.jpg", "img10.jpg"]
        );
    }

    #[test]
    fn folders_lead_whatever_the_column_and_which_way() {
        for key in SortKey::ALL {
            for descending in [false, true] {
                let names = sorted(Order { key, descending });
                assert!(
                    names[..2].iter().all(|n| n.ends_with("dir")),
                    "{key:?} {descending}: {names:?}"
                );
                if key == SortKey::Size {
                    assert_eq!(names[..2], ["adir", "zdir"], "by name, either way");
                }
            }
        }
    }

    #[test]
    fn size_and_time_break_their_ties_by_name() {
        let by_size = Order {
            key: SortKey::Size,
            descending: false,
        };
        // Folders by name, then files smallest first, img10 and b.jpg tied
        // on size and put in name order.
        assert_eq!(
            sorted(by_size),
            ["adir", "zdir", "a.jpg", "img9.jpg", "b.jpg", "img10.jpg"]
        );
        let newest = Order {
            key: SortKey::Modified,
            descending: true,
        };
        assert_eq!(
            sorted(newest),
            ["adir", "zdir", "a.jpg", "img10.jpg", "img9.jpg", "b.jpg"]
        );
    }

    #[test]
    fn a_heading_clicked_twice_turns_the_order_round() {
        let name = Order::default();
        let size = name.by(SortKey::Size);
        assert_eq!(
            size,
            Order {
                key: SortKey::Size,
                descending: true
            },
            "biggest first"
        );
        assert!(!size.by(SortKey::Size).descending);
        assert_eq!(name.by(SortKey::Name), name.reversed());
        assert!(name.reversed().descending);
        // Stepping through the columns comes back round to names.
        let back = name.next_key().next_key().next_key();
        assert_eq!(back.key, SortKey::Name);
    }

    #[test]
    fn a_gone_folder_left_its_place_among_the_folders_by_name() {
        let listing = |order: Order| {
            let mut v = sample();
            v.push(entry("mdir", FsKind::Dir, 0, 0));
            v.sort_by(|a, b| order.cmp(a, b));
            v
        };
        let by_name = Order::default();
        // adir, mdir, zdir, then the files.
        assert_eq!(by_name.folder_slot("kdir", &listing(by_name)), Some(1));
        assert_eq!(by_name.folder_slot("a", &listing(by_name)), Some(0));
        assert_eq!(
            by_name.folder_slot("zz", &listing(by_name)),
            Some(3),
            "after the last folder: the first file"
        );
        // Natural order, as the listing has: dir9 comes before dir10.
        let numbered: Vec<FsEntry> = ["dir2", "dir10"]
            .iter()
            .map(|n| entry(n, FsKind::Dir, 0, 0))
            .collect();
        assert_eq!(by_name.folder_slot("dir9", &numbered), Some(1));

        let z_to_a = by_name.reversed();
        assert_eq!(z_to_a.folder_slot("kdir", &listing(z_to_a)), Some(2));
        // Folders by size stay A to Z whichever way the files go.
        let biggest = by_name.by(SortKey::Size);
        assert_eq!(biggest.folder_slot("kdir", &listing(biggest)), Some(1));
        let newest = by_name.by(SortKey::Modified);
        assert_eq!(newest.folder_slot("kdir", &listing(newest)), None);
    }

    #[test]
    fn keys_read_back() {
        for k in SortKey::ALL {
            assert_eq!(SortKey::from_key(k.key()), Some(k));
        }
        assert_eq!(SortKey::from_key("colour"), None);
    }
}
