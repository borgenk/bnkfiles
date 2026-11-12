//! What the inspector says about the thing in hand: one entry, several, or
//! the open folder when nothing is selected.
//!
//! Pure: it reads only what the listing and the image store already hold, so
//! the panel never asks the disk for anything. A picture's dimensions are
//! known once a full decode of it exists and are left out until then.

use std::path::Path;

use crate::explorer::{format_size, format_stamp};
use crate::files::{self, FsEntry};

/// What the inspector is describing.
pub enum Subject<'a> {
    /// The folder being shown, with nothing selected in it.
    Folder { dir: &'a Path, items: usize },
    /// One entry, and a picture's dimensions if a decode has them.
    One {
        entry: &'a FsEntry,
        dims: Option<(u32, u32)>,
    },
    /// Several entries: how many, and the bytes of the files among them.
    Many { count: usize, bytes: u64 },
}

/// The panel's text: a title and its property rows, label then value.
#[derive(Debug, PartialEq, Eq)]
pub struct Description {
    pub title: String,
    pub badge: Option<String>,
    pub rows: Vec<(&'static str, String)>,
}

/// What `entry` is, in the words a person would use, and the short badge its
/// preview carries.
fn kind_of(entry: &FsEntry) -> (String, String) {
    if entry.is_dir() {
        return ("Folder".to_string(), "DIR".to_string());
    }
    let ext = entry
        .path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_uppercase);
    match (ext, files::is_image(&entry.path)) {
        (Some(ext), true) => (format!("{ext} image"), ext),
        (Some(ext), false) => (format!("{ext} file"), ext),
        (None, _) => ("File".to_string(), "FILE".to_string()),
    }
}

/// `local` turns a unix time into the viewer's zone, so this stays pure.
pub fn describe(subject: &Subject, local: &dyn Fn(i64) -> i64) -> Description {
    match subject {
        Subject::Folder { dir, items } => Description {
            title: folder_name(dir),
            badge: Some("DIR".to_string()),
            rows: vec![
                ("Kind", "Folder".to_string()),
                ("Items", items.to_string()),
                ("Location", location(dir)),
            ],
        },
        Subject::One { entry, dims } => {
            let (kind, badge) = kind_of(entry);
            let mut rows = vec![("Kind", kind)];
            if !entry.is_dir() {
                rows.push(("Size", format_size(entry.size)));
            }
            if let Some((w, h)) = dims {
                rows.push(("Dimensions", format!("{w} × {h}")));
            }
            rows.push(("Modified", format_stamp(local(entry.mtime_secs))));
            rows.push(("Location", location(&entry.path)));
            Description {
                title: entry.name.clone(),
                badge: Some(badge),
                rows,
            }
        }
        Subject::Many { count, bytes } => Description {
            title: format!("{count} items"),
            badge: None,
            rows: vec![("Total size", format_size(*bytes))],
        },
    }
}

/// A folder's own name, or `/` for the root, which has none.
fn folder_name(dir: &Path) -> String {
    match dir.file_name() {
        Some(_) => files::file_name(dir),
        None => dir.display().to_string(),
    }
}

/// The folder something is in, as a path.
fn location(path: &Path) -> String {
    path.parent()
        .map_or_else(String::new, |p| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::files::FsKind;
    use crate::inspector::*;

    fn entry(path: &str, kind: FsKind, size: u64) -> FsEntry {
        let path = PathBuf::from(path);
        FsEntry {
            name: files::file_name(&path),
            path,
            kind,
            size,
            mtime_secs: 1_000_000_000,
            hidden: false,
        }
    }

    fn utc(secs: i64) -> i64 {
        secs
    }

    #[test]
    fn a_picture_reads_as_its_kind_size_and_place() {
        let e = entry("/pics/beach.jpg", FsKind::File, 2 * 1024 * 1024);
        let d = describe(
            &Subject::One {
                entry: &e,
                dims: None,
            },
            &utc,
        );
        assert_eq!(d.title, "beach.jpg");
        assert_eq!(d.badge.as_deref(), Some("JPG"));
        assert_eq!(
            d.rows,
            vec![
                ("Kind", "JPG image".to_string()),
                ("Size", "2.0 MB".to_string()),
                ("Modified", "2001-09-09 01:46".to_string()),
                ("Location", "/pics".to_string()),
            ]
        );
    }

    #[test]
    fn dimensions_appear_only_once_a_decode_has_them() {
        let e = entry("/pics/beach.jpg", FsKind::File, 10);
        let without = describe(
            &Subject::One {
                entry: &e,
                dims: None,
            },
            &utc,
        );
        assert!(without.rows.iter().all(|(label, _)| *label != "Dimensions"));
        let with = describe(
            &Subject::One {
                entry: &e,
                dims: Some((4000, 3000)),
            },
            &utc,
        );
        assert!(with
            .rows
            .contains(&("Dimensions", "4000 × 3000".to_string())));
    }

    #[test]
    fn a_folder_entry_has_no_size_and_the_open_folder_counts_its_items() {
        let e = entry("/home/me", FsKind::Dir, 4096);
        let d = describe(
            &Subject::One {
                entry: &e,
                dims: None,
            },
            &utc,
        );
        assert_eq!(d.rows[0], ("Kind", "Folder".to_string()));
        assert!(d.rows.iter().all(|(label, _)| *label != "Size"));

        let d = describe(
            &Subject::Folder {
                dir: Path::new("/home/me"),
                items: 12,
            },
            &utc,
        );
        assert_eq!(d.title, "me");
        assert!(d.rows.contains(&("Items", "12".to_string())));
        assert!(d.rows.contains(&("Location", "/home".to_string())));
        // The root has no name of its own.
        let root = describe(
            &Subject::Folder {
                dir: Path::new("/"),
                items: 3,
            },
            &utc,
        );
        assert_eq!(root.title, "/");
    }

    #[test]
    fn a_selection_reads_as_a_count_and_a_total() {
        let d = describe(
            &Subject::Many {
                count: 3,
                bytes: 3 * 1024,
            },
            &utc,
        );
        assert_eq!(d.title, "3 items");
        assert_eq!(d.rows, vec![("Total size", "3.0 KB".to_string())]);
    }

    #[test]
    fn a_file_with_no_extension_is_a_file() {
        let e = entry("/etc/hostname", FsKind::File, 7);
        let d = describe(
            &Subject::One {
                entry: &e,
                dims: None,
            },
            &utc,
        );
        assert_eq!(d.badge.as_deref(), Some("FILE"));
        assert_eq!(d.rows[0], ("Kind", "File".to_string()));
    }
}
