//! Persisted settings: a tiny `key=value` file under `~/.config/bnkfiles/`.
//!
//! A small parser of its own rather than `serde`/`dirs`, in keeping with the
//! project's dependency budget. Loading is forgiving: anything missing or
//! malformed falls back to a default, so a corrupt file never stops the app.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::masonry::Shape;
use crate::order::{Order, SortKey};
use crate::sidebar::Section;

const DEFAULT_W: u32 = 900;
const DEFAULT_H: u32 = 700;
/// Clamp persisted window dimensions so a corrupt file cannot open a zero-size
/// or absurd window.
const MIN_DIM: u32 = 100;
const MAX_DIM: u32 = 16384;

/// Window geometry, the last-viewed location, and the explorer's preferences,
/// restored across runs.
pub struct Settings {
    pub width: u32,
    pub height: u32,
    pub last_dir: Option<PathBuf>,
    pub last_index: usize,
    /// Whether the explorer was last showing thumbnails rather than the detail
    /// list. Reopening in the other one is a small thing that is annoying daily.
    pub grid_view: bool,
    /// Whether dot-files were being shown.
    pub show_hidden: bool,
    /// Whether the inspector was open in a plain folder. Off by default: in most
    /// folders it would be mostly empty.
    pub inspector: bool,
    /// Folders pinned to the places sidebar, in the order they were added. One
    /// `pin=` line each, so the file stays flat `key=value`.
    pub pinned: Vec<PathBuf>,
    /// The sidebar sections folded shut, as one `folded=` line.
    pub folded: Vec<Section>,
    /// The collections seen so far: folders with a `.bnkfiles/` marker. One
    /// `collection=` line each.
    pub collections: Vec<PathBuf>,
    /// The order folders are listed in.
    pub order: Order,
    /// The thumbnail shape chosen for folders, oldest choice first. One
    /// `shape=` line each, the shape's name and then the folder. A folder with
    /// none is laid out as `auto` says.
    pub shapes: Vec<(PathBuf, Shape)>,
}

/// How many folders' shapes are remembered. A choice is a key press away, so
/// past this the oldest are let go rather than letting the file grow forever.
pub const MAX_SHAPES: usize = 256;

impl Default for Settings {
    fn default() -> Self {
        Self {
            width: DEFAULT_W,
            height: DEFAULT_H,
            last_dir: None,
            last_index: 0,
            grid_view: false,
            show_hidden: false,
            inspector: false,
            pinned: Vec::new(),
            folded: Vec::new(),
            collections: Vec::new(),
            order: Order::default(),
            shapes: Vec::new(),
        }
    }
}

impl Settings {
    /// Load settings from disk, or defaults if absent/unreadable.
    pub fn load() -> Self {
        config_path().map_or_else(Settings::default, |path| load_from(&path))
    }

    /// Change the settings on disk and return what they now are. Best-effort: on a
    /// machine with no config directory the change is made to defaults and kept
    /// nowhere.
    ///
    /// The file is read again first and `change` is applied to that, not to what
    /// this process loaded at startup. Two windows share the one file, and writing
    /// back a whole remembered copy would undo whatever the other had changed
    /// since: a folder pinned in one would vanish when the other closed.
    pub fn update(change: impl FnOnce(&mut Settings)) -> std::io::Result<Self> {
        match config_path() {
            Some(path) => update_at(&path, change),
            None => {
                let mut settings = Settings::default();
                change(&mut settings);
                Ok(settings)
            }
        }
    }

    /// The file's text: what [`parse`] reads back into these same settings.
    fn render(&self) -> String {
        let mut out = format!(
            "width={}\nheight={}\nlast_index={}\ngrid_view={}\nshow_hidden={}\ninspector={}\n",
            self.width,
            self.height,
            self.last_index,
            self.grid_view,
            self.show_hidden,
            self.inspector
        );
        if let Some(dir) = self.last_dir.as_deref().and_then(storable) {
            out.push_str(&format!("last_dir={dir}\n"));
        }
        for pin in self.pinned.iter().filter_map(|p| storable(p)) {
            out.push_str(&format!("pin={pin}\n"));
        }
        out.push_str(&format!(
            "sort={}\nsort_descending={}\n",
            self.order.key.key(),
            self.order.descending
        ));
        if !self.folded.is_empty() {
            let keys: Vec<&str> = self.folded.iter().map(|s| s.key()).collect();
            out.push_str(&format!("folded={}\n", keys.join(",")));
        }
        for root in self.collections.iter().filter_map(|p| storable(p)) {
            out.push_str(&format!("collection={root}\n"));
        }
        for (dir, shape) in &self.shapes {
            if let Some(dir) = storable(dir) {
                out.push_str(&format!("shape={} {dir}\n", shape.key()));
            }
        }
        out
    }
}

/// The settings in the file at `path`, or defaults if it is absent or unreadable.
fn load_from(path: &Path) -> Settings {
    std::fs::read_to_string(path).map_or_else(|_| Settings::default(), |text| parse(&text))
}

/// [`Settings::update`] on the file at `path`, creating its directory if needed.
fn update_at(path: &Path, change: impl FnOnce(&mut Settings)) -> std::io::Result<Settings> {
    let mut settings = load_from(path);
    change(&mut settings);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(path, settings.render().as_bytes())?;
    Ok(settings)
}

/// A path as it can be written on one line of the file, or `None` for one that
/// cannot: not UTF-8, or holding a line break. Such a path is skipped rather than
/// written lossily, since a line naming a directory that does not exist is worse
/// than a pin that has to be made again.
fn storable(path: &Path) -> Option<&str> {
    path.to_str().filter(|text| !text.contains(['\n', '\r']))
}

/// Write `bytes` to `path` atomically: a same-directory temp file, flushed to disk
/// and then renamed over the target. A crash mid-write, or another run reading the
/// file at the same moment, sees either the old config or the new one whole, never a
/// half-written truncation. The temp file shares the target's directory so the
/// rename stays on one filesystem (a cross-device rename is not atomic), and carries
/// the pid so two instances saving at once do not stage over each other.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("config path has no file name"))?;
    let mut tmp_name = OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);

    let staged = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        // fsync the contents before the rename, so a crash cannot leave the new name
        // pointing at data the disk has not actually committed yet.
        f.sync_all()
    })();
    match staged.and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Do not leave the staging file behind on failure.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Parse the `key=value` body, ignoring blank lines, comments, and junk.
fn parse(text: &str) -> Settings {
    let mut s = Settings::default();
    for line in text.split('\n') {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        // Numbers and flags are trimmed. A path is taken exactly as written: a
        // directory name may begin or end with a space, and trimming it names a
        // different directory.
        let value = raw.trim();
        match key.trim() {
            "width" => {
                if let Ok(w) = value.parse::<u32>() {
                    s.width = w.clamp(MIN_DIM, MAX_DIM);
                }
            }
            "height" => {
                if let Ok(h) = value.parse::<u32>() {
                    s.height = h.clamp(MIN_DIM, MAX_DIM);
                }
            }
            "last_index" => {
                if let Ok(i) = value.parse::<usize>() {
                    s.last_index = i;
                }
            }
            "last_dir" if !raw.is_empty() => {
                s.last_dir = Some(PathBuf::from(raw));
            }
            "grid_view" => s.grid_view = value == "true",
            "show_hidden" => s.show_hidden = value == "true",
            "inspector" => s.inspector = value == "true",
            // Repeated, once per pinned folder, in sidebar order.
            "pin" if !raw.is_empty() => s.pinned.push(PathBuf::from(raw)),
            // A name this build does not know is dropped, not kept as junk.
            "folded" => {
                s.folded = value
                    .split(',')
                    .filter_map(|key| Section::from_key(key.trim()))
                    .collect();
            }
            "collection" if !raw.is_empty() => s.collections.push(PathBuf::from(raw)),
            "sort" => {
                if let Some(key) = SortKey::from_key(value) {
                    s.order.key = key;
                }
            }
            "sort_descending" => s.order.descending = value == "true",
            "shape" => {
                if let Some((shape, dir)) = raw.split_once(' ') {
                    if let (Some(shape), false) = (Shape::from_key(shape), dir.is_empty()) {
                        s.shapes.push((PathBuf::from(dir), shape));
                    }
                }
            }
            _ => {}
        }
    }
    s
}

/// `$XDG_CONFIG_HOME/bnkfiles/config`, falling back to `$HOME/.config/...`.
fn config_path() -> Option<PathBuf> {
    let mut base = if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        let p = PathBuf::from(xdg);
        if p.is_absolute() {
            Some(p)
        } else {
            None
        }
    } else {
        None
    };
    if base.is_none() {
        let mut home = PathBuf::from(std::env::var_os("HOME")?);
        home.push(".config");
        base = Some(home);
    }
    let mut path = base?;
    path.push("bnkfiles");
    path.push("config");
    Some(path)
}

#[cfg(test)]
mod tests {
    use crate::settings::*;

    #[test]
    fn parses_known_keys() {
        let s = parse("width=1280\nheight=720\nlast_index=4\nlast_dir=/home/u/pics\n");
        assert_eq!(s.width, 1280);
        assert_eq!(s.height, 720);
        assert_eq!(s.last_index, 4);
        assert_eq!(s.last_dir, Some(PathBuf::from("/home/u/pics")));
    }

    #[test]
    fn the_explorers_preferences_survive_a_round_trip() {
        let s = parse("grid_view=true\nshow_hidden=true\npin=/home/u/pics\npin=/mnt/archive\n");
        assert!(s.grid_view);
        assert!(s.show_hidden);
        assert_eq!(
            s.pinned,
            vec![PathBuf::from("/home/u/pics"), PathBuf::from("/mnt/archive")],
            "pins keep the order they were written in"
        );

        // Anything that is not the word "true" is false, so a corrupt or
        // hand-edited file cannot leave a flag in some third state.
        let s = parse("grid_view=yes\nshow_hidden=1\n");
        assert!(!s.grid_view);
        assert!(!s.show_hidden);
        // An empty pin names no directory and is not one.
        assert!(parse("pin=\n").pinned.is_empty());
    }

    #[test]
    fn folded_sections_and_collections_survive_a_round_trip() {
        let saved = Settings {
            folded: vec![Section::Devices, Section::Collections],
            collections: vec![PathBuf::from("/home/u/refs"), PathBuf::from("/mnt/a b")],
            ..Settings::default()
        };
        let back = parse(&saved.render());
        assert_eq!(back.folded, saved.folded);
        assert_eq!(back.collections, saved.collections);
        // A section name from some other build is dropped, the rest kept.
        assert_eq!(
            parse("folded=pinned,bogus,places\n").folded,
            [Section::Pinned, Section::Places]
        );
        assert!(parse("folded=\n").folded.is_empty());
    }

    #[test]
    fn folder_shapes_survive_a_round_trip_and_junk_is_dropped() {
        let saved = Settings {
            shapes: vec![
                (PathBuf::from("/home/u/photos"), Shape::Masonry),
                (PathBuf::from("/mnt/icons and more"), Shape::Square),
            ],
            ..Settings::default()
        };
        assert_eq!(parse(&saved.render()).shapes, saved.shapes);
        let junk = parse("shape=justified /a\nshape=square\nshape=square \nshape=auto /b\n");
        assert_eq!(junk.shapes, [(PathBuf::from("/b"), Shape::Auto)]);
    }

    #[test]
    fn the_sort_order_survives_a_round_trip_and_junk_keeps_the_default() {
        let saved = Settings {
            order: Order {
                key: SortKey::Modified,
                descending: true,
            },
            ..Settings::default()
        };
        assert_eq!(parse(&saved.render()).order, saved.order);
        assert_eq!(parse("sort=colour\n").order, Order::default());
    }

    #[test]
    fn ignores_junk_and_clamps() {
        let s = parse("# a comment\nwidth=oops\nheight=999999\n\nbogus line\nwidth=50\n");
        // width: the bad parse is skipped, then 50 is clamped up to MIN_DIM.
        assert_eq!(s.width, MIN_DIM);
        // height clamps down to MAX_DIM.
        assert_eq!(s.height, MAX_DIM);
        // untouched keys keep defaults.
        assert_eq!(s.last_index, 0);
        assert_eq!(s.last_dir, None);
    }

    #[test]
    fn empty_input_is_default() {
        let s = parse("");
        assert_eq!(s.width, DEFAULT_W);
        assert_eq!(s.height, DEFAULT_H);
        assert!(s.last_dir.is_none());
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp() {
        // A unique directory under the system temp dir; cleaned up at the end.
        let dir = std::env::temp_dir().join(format!("bnkfiles-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("make temp dir");
        let path = dir.join("config");

        write_atomic(&path, b"first").expect("first write");
        assert_eq!(std::fs::read(&path).expect("read"), b"first");

        // A second write replaces the contents in place, whole.
        write_atomic(&path, b"second value").expect("second write");
        assert_eq!(std::fs::read(&path).expect("read"), b"second value");

        // The staging file must not survive a successful write: the directory holds
        // only the target.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .filter(|n| n != "config")
            .collect();
        assert!(leftovers.is_empty(), "unexpected files left: {leftovers:?}");

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_path_is_read_back_exactly_as_it_was_written() {
        let odd = PathBuf::from("/home/u/ trailing space ");
        let saved = Settings {
            last_dir: Some(odd.clone()),
            pinned: vec![
                odd.clone(),
                PathBuf::from("/a=b"),
                PathBuf::from("/two\nlines"),
            ],
            ..Settings::default()
        };
        let loaded = parse(&saved.render());
        assert_eq!(loaded.last_dir, Some(odd.clone()));
        assert_eq!(
            loaded.pinned,
            vec![odd, PathBuf::from("/a=b")],
            "a name with a line break cannot be a line, so it is left out whole"
        );
    }

    /// Two windows share one file. A window that closes writes its size and
    /// folder; it must not also write back the pins it started with, or a folder
    /// pinned in the other window meanwhile is gone.
    #[test]
    fn one_windows_change_does_not_undo_anothers() {
        let dir = std::env::temp_dir().join(format!("bnkfiles-settings-{}", std::process::id()));
        let path = dir.join("nested").join("config");
        let _ = std::fs::remove_dir_all(&dir);

        // The first window pins a folder; the second, opened before that, closes.
        update_at(&path, |s| s.pinned.push(PathBuf::from("/photos"))).expect("pin");
        let closed = update_at(&path, |s| s.width = 1234).expect("close");
        assert_eq!(closed.pinned, vec![PathBuf::from("/photos")]);

        let on_disk = load_from(&path);
        std::fs::remove_dir_all(&dir).expect("clean up");
        assert_eq!(on_disk.width, 1234);
        assert_eq!(on_disk.pinned, vec![PathBuf::from("/photos")]);
    }
}
