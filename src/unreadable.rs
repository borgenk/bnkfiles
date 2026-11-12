//! A folder that could not be read: why, in words a person reads, and the
//! panel the explorer shows where its entries would have been.
//!
//! The scan worker decides why, where the failure happens, and finds the
//! nearest folder above it that is still there, since both are questions
//! for the disk and the event loop does not ask those. What is left here is
//! pure: the words for each reason and where the panel's lines land, which
//! the painter and the click handler both read.
//!
//! ```text
//!                  This folder doesn't exist                  headline
//!      /home/me/collections/test/photography/night            path
//!           No such file or directory (os error 2)            detail
//!
//!              ┌─────────┐
//!              │Backspace│  Go to photography                 actions: a key and
//!              │  Alt+←  │  Back                              what it does; a
//!              │   F5    │  Try again                         click runs it too
//!              └─────────┘
//! ```

use std::io;
use std::path::{Path, PathBuf};

use crate::explorer;
use crate::files;
use crate::platform::geom::{Rect, Scale};

/// Linux errno values the standard library has no kind of its own for, and
/// that mean the device under the path has gone rather than the path itself.
const ENXIO: i32 = 6;
const ENODEV: i32 = 19;
const EHOSTDOWN: i32 = 112;
const ENOMEDIUM: i32 = 123;

/// Why a folder could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Nothing is at the path, or something on the way to it is gone.
    Missing,
    /// It is there, and this user may not read it.
    Denied,
    /// The path names a file.
    NotFolder,
    /// The drive or share it lives on is not answering.
    Unavailable,
    /// Anything else, a listing that broke off part way among them.
    Failed,
}

impl Reason {
    /// The reason an error from reading a folder stands for.
    pub fn of(e: &io::Error) -> Reason {
        match e.kind() {
            io::ErrorKind::NotFound => Reason::Missing,
            io::ErrorKind::PermissionDenied => Reason::Denied,
            io::ErrorKind::NotADirectory => Reason::NotFolder,
            io::ErrorKind::NotConnected
            | io::ErrorKind::StaleNetworkFileHandle
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::NetworkDown
            | io::ErrorKind::TimedOut => Reason::Unavailable,
            _ => match e.raw_os_error() {
                Some(ENXIO | ENODEV | EHOSTDOWN | ENOMEDIUM) => Reason::Unavailable,
                _ => Reason::Failed,
            },
        }
    }

    /// What the panel says first, in a sentence.
    pub fn headline(self) -> &'static str {
        match self {
            Reason::Missing => "This folder doesn't exist",
            Reason::Denied => "You don't have permission to open this folder",
            Reason::NotFolder => "This is a file, not a folder",
            Reason::Unavailable => "The drive or share this folder is on isn't available",
            Reason::Failed => "This folder couldn't be read",
        }
    }

    /// The reason's name on the probe line.
    pub fn key(self) -> &'static str {
        match self {
            Reason::Missing => "missing",
            Reason::Denied => "denied",
            Reason::NotFolder => "not-folder",
            Reason::Unavailable => "unavailable",
            Reason::Failed => "failed",
        }
    }
}

/// A folder the scan worker could not read, or not read to the end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unreadable {
    pub reason: Reason,
    /// The system's own words for what went wrong: the panel's small print.
    pub detail: String,
    /// Some entries were read before the listing broke off. They are shown,
    /// and the folder is not all there.
    pub partial: bool,
    /// The nearest folder above it that is still there, when the folder
    /// itself could not be opened: where Up goes from it.
    pub nearest: Option<PathBuf>,
}

impl Unreadable {
    /// Whether the folder at `target`, the folder that could not be read or
    /// one of the folders on the way to it, is gone: below the nearest one
    /// still there, when it is not there or is a file now. The breadcrumb
    /// dims those.
    pub fn is_gone(&self, target: &Path) -> bool {
        matches!(self.reason, Reason::Missing | Reason::NotFolder)
            && self
                .nearest
                .as_ref()
                .is_some_and(|n| !n.starts_with(target))
    }
}

/// What the panel's actions do. Each is also a key, which the panel names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// To the nearest folder above that is there.
    Up,
    Back,
    Retry,
}

/// Which font a line of the panel is set in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Headline,
    Text,
    Small,
}

/// One line of text and where it goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub rect: Rect,
    pub style: Style,
}

/// One action: its key in a cap, then what it does. `rect` is the whole
/// row, which is what a click is tested against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionRow {
    pub action: Action,
    pub key: Line,
    pub cap: Rect,
    pub label: Line,
    pub rect: Rect,
}

/// The panel, laid out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Panel {
    pub lines: Vec<Line>,
    pub actions: Vec<ActionRow>,
}

impl Panel {
    /// The action at window point `(px, py)`, if a row is there.
    pub fn action_at(&self, px: f32, py: f32) -> Option<Action> {
        self.actions
            .iter()
            .find(|row| explorer::contains(row.rect, px, py))
            .map(|row| row.action)
    }
}

/// The fonts, as the layout needs them: each style's line height, and a
/// width for a string set in it.
pub struct Fonts<'a> {
    pub line_height: &'a dyn Fn(Style) -> i32,
    pub measure: &'a dyn Fn(Style, &str) -> i32,
}

/// Logical pixels.
const PAD: i32 = 24;
const CAP_PAD: i32 = 8;
const ROW_GAP: i32 = 6;
const LABEL_GAP: i32 = 12;

/// What the panel is about, and what it offers.
pub struct Contents<'a> {
    pub dir: &'a Path,
    pub failure: &'a Unreadable,
    /// The actions, in the order offered, each with the key that does it.
    pub actions: &'a [(Action, String)],
    /// Where Up goes, which its words name.
    pub up_to: Option<&'a Path>,
}

/// Lay the panel out in `content`.
///
/// The block is centred across the content and set a third of the way down
/// it, where the eye lands in an empty pane; each line is cut short in the
/// middle rather than run past the edges, which keeps a long path's both
/// ends readable.
pub fn panel(content: Rect, scale: Scale, what: &Contents, fonts: &Fonts) -> Panel {
    let Contents {
        dir,
        failure,
        actions,
        up_to,
    } = *what;
    let pad = scale.px(PAD);
    let room = (content.w - 2 * pad).max(1);
    let fit = |style: Style, text: &str| {
        let text = explorer::truncate_middle(text, room, &|t| (fonts.measure)(style, t));
        let w = (fonts.measure)(style, &text).min(room);
        (text, w)
    };

    let mut texts = vec![
        fit(Style::Headline, failure.reason.headline()),
        fit(Style::Text, &dir.to_string_lossy()),
    ];
    if !failure.detail.is_empty() {
        texts.push(fit(Style::Small, &failure.detail));
    }
    let styles = [Style::Headline, Style::Text, Style::Small];

    let label_of = |action: Action| match action {
        Action::Up => format!("Go to {}", up_to.map_or_else(String::new, files::file_name)),
        Action::Back => "Back".to_string(),
        Action::Retry => "Try again".to_string(),
    };
    let caps: Vec<(String, String)> = actions
        .iter()
        .map(|(action, key)| (key.clone(), label_of(*action)))
        .collect();
    let cap_pad = scale.px(CAP_PAD);
    let cap_w = caps
        .iter()
        .map(|(key, _)| (fonts.measure)(Style::Small, key))
        .max()
        .unwrap_or(0)
        + 2 * cap_pad;
    let label_gap = scale.px(LABEL_GAP);
    let label_room = (room - cap_w - label_gap).max(1);
    let labels: Vec<(String, i32)> = caps
        .iter()
        .map(|(_, label)| {
            let text =
                explorer::truncate_middle(label, label_room, &|t| (fonts.measure)(Style::Text, t));
            let w = (fonts.measure)(Style::Text, &text).min(label_room);
            (text, w)
        })
        .collect();
    let label_w = labels.iter().map(|(_, w)| *w).max().unwrap_or(0);
    let row_h = (fonts.line_height)(Style::Text) + scale.px(ROW_GAP);
    let block_w = cap_w + label_gap + label_w;

    let line_gap = scale.px(4);
    let before_actions = scale.px(20);
    let text_h: i32 = texts
        .iter()
        .zip(styles)
        .map(|(_, style)| (fonts.line_height)(style) + line_gap)
        .sum();
    let actions_h = if actions.is_empty() {
        0
    } else {
        before_actions + row_h * actions.len() as i32
    };
    let block_h = text_h + actions_h;
    let mut y = content.y + ((content.h - block_h) / 3).max(pad);

    let centre = |w: i32| content.x + (content.w - w) / 2;
    let mut lines = Vec::new();
    for ((text, w), style) in texts.into_iter().zip(styles) {
        let h = (fonts.line_height)(style);
        lines.push(Line {
            text,
            rect: Rect {
                x: centre(w),
                y,
                w,
                h,
            },
            style,
        });
        y += h + line_gap;
    }

    y += before_actions;
    let block_x = centre(block_w);
    let mut rows = Vec::new();
    for (i, (&(action, _), (key, _))) in actions.iter().zip(&caps).enumerate() {
        let (label, w) = labels.get(i).cloned().unwrap_or_default();
        let key_w = (fonts.measure)(Style::Small, key);
        let small_h = (fonts.line_height)(Style::Small);
        let text_h = (fonts.line_height)(Style::Text);
        let cap_h = (small_h + scale.px(4)).min(row_h);
        let cap = Rect {
            x: block_x,
            y: y + (row_h - cap_h) / 2,
            w: cap_w,
            h: cap_h,
        };
        rows.push(ActionRow {
            action,
            key: Line {
                text: key.clone(),
                rect: Rect {
                    x: cap.x + (cap_w - key_w) / 2,
                    y: y + (row_h - small_h) / 2,
                    w: key_w,
                    h: small_h,
                },
                style: Style::Small,
            },
            cap,
            label: Line {
                text: label,
                rect: Rect {
                    x: block_x + cap_w + label_gap,
                    y: y + (row_h - text_h) / 2,
                    w,
                    h: text_h,
                },
                style: Style::Text,
            },
            rect: Rect {
                x: block_x,
                y,
                w: block_w,
                h: row_h,
            },
        });
        y += row_h;
    }
    Panel {
        lines,
        actions: rows,
    }
}

#[cfg(test)]
mod tests {
    use crate::unreadable::*;

    fn gone(nearest: &str) -> Unreadable {
        Unreadable {
            reason: Reason::Missing,
            detail: "No such file or directory (os error 2)".to_string(),
            partial: false,
            nearest: Some(PathBuf::from(nearest)),
        }
    }

    /// Ten pixels a character and line heights by style: enough to lay out
    /// against without a font.
    fn layout(content: Rect, failure: &Unreadable, actions: &[Action]) -> Panel {
        let height = |style: Style| match style {
            Style::Headline => 24,
            Style::Text => 20,
            Style::Small => 16,
        };
        let measure = |_: Style, text: &str| 10 * text.chars().count() as i32;
        let actions: Vec<(Action, String)> = actions
            .iter()
            .map(|&action| {
                let key = match action {
                    Action::Up => "Backspace",
                    Action::Back => "Alt+←",
                    Action::Retry => "F5",
                };
                (action, key.to_string())
            })
            .collect();
        panel(
            content,
            Scale::ONE,
            &Contents {
                dir: Path::new("/home/me/photos/night"),
                failure,
                actions: &actions,
                up_to: failure.nearest.as_deref(),
            },
            &Fonts {
                line_height: &height,
                measure: &measure,
            },
        )
    }

    const CONTENT: Rect = Rect {
        x: 200,
        y: 44,
        w: 800,
        h: 600,
    };

    #[test]
    fn errors_are_sorted_into_what_a_person_can_do_about_them() {
        let os = io::Error::from_raw_os_error;
        assert_eq!(Reason::of(&os(2)), Reason::Missing, "ENOENT");
        assert_eq!(Reason::of(&os(13)), Reason::Denied, "EACCES");
        assert_eq!(Reason::of(&os(1)), Reason::Denied, "EPERM");
        assert_eq!(Reason::of(&os(20)), Reason::NotFolder, "ENOTDIR");
        for (errno, name) in [
            (107, "ENOTCONN, a FUSE mount whose daemon died"),
            (116, "ESTALE, an NFS export that went away"),
            (6, "ENXIO"),
            (19, "ENODEV"),
            (112, "EHOSTDOWN"),
            (123, "ENOMEDIUM, a card reader with no card"),
        ] {
            assert_eq!(Reason::of(&os(errno)), Reason::Unavailable, "{name}");
        }
        assert_eq!(Reason::of(&os(5)), Reason::Failed, "EIO says nothing more");
    }

    #[test]
    fn the_folders_below_the_nearest_one_still_there_are_gone() {
        let failure = gone("/home/me");
        assert!(!failure.is_gone(Path::new("/")));
        assert!(!failure.is_gone(Path::new("/home")));
        assert!(!failure.is_gone(Path::new("/home/me")));
        assert!(failure.is_gone(Path::new("/home/me/photos")));
        assert!(failure.is_gone(Path::new("/home/me/photos/night")));

        // A folder that is there but cannot be read is not gone.
        let denied = Unreadable {
            reason: Reason::Denied,
            ..failure
        };
        assert!(!denied.is_gone(Path::new("/home/me/photos/night")));
    }

    #[test]
    fn the_panel_is_centred_and_its_actions_stack_without_overlapping() {
        let failure = gone("/home/me/photos");
        let p = layout(
            CONTENT,
            &failure,
            &[Action::Up, Action::Back, Action::Retry],
        );
        assert_eq!(
            p.lines.iter().map(|l| l.style).collect::<Vec<_>>(),
            [Style::Headline, Style::Text, Style::Small]
        );
        assert_eq!(p.lines[0].text, "This folder doesn't exist");
        assert_eq!(p.lines[1].text, "/home/me/photos/night");
        let mid = CONTENT.x + CONTENT.w / 2;
        for line in &p.lines {
            let centre = line.rect.x + line.rect.w / 2;
            assert!((centre - mid).abs() <= 1, "{line:?} is centred");
        }
        // Each line below the one before.
        for pair in p.lines.windows(2) {
            assert!(pair[1].rect.y >= pair[0].rect.y + pair[0].rect.h);
        }

        assert_eq!(
            p.actions.iter().map(|a| a.action).collect::<Vec<_>>(),
            [Action::Up, Action::Back, Action::Retry]
        );
        assert_eq!(p.actions[0].label.text, "Go to photos");
        let first = &p.actions[0];
        let last = p.lines.last().expect("a line");
        assert!(first.rect.y > last.rect.y + last.rect.h, "under the text");
        for pair in p.actions.windows(2) {
            assert_eq!(pair[1].rect.y, pair[0].rect.y + pair[0].rect.h);
            assert_eq!(pair[0].cap.w, pair[1].cap.w, "one width of cap");
            assert_eq!(pair[0].label.rect.x, pair[1].label.rect.x, "labels align");
        }
        for row in &p.actions {
            assert!(row.cap.x >= row.rect.x && row.cap.x + row.cap.w <= row.label.rect.x);
            assert!(row.key.rect.x >= row.cap.x && row.key.rect.w <= row.cap.w);
        }
        assert!(
            (first.rect.x + first.rect.w / 2 - mid).abs() <= 1,
            "the actions centre as a block"
        );
    }

    #[test]
    fn a_click_finds_the_action_on_its_row_and_nothing_between() {
        let failure = gone("/home/me/photos");
        let p = layout(CONTENT, &failure, &[Action::Up, Action::Retry]);
        for row in &p.actions {
            let (x, y) = (
                (row.rect.x + row.rect.w - 1) as f32,
                (row.rect.y + row.rect.h / 2) as f32,
            );
            assert_eq!(p.action_at(x, y), Some(row.action));
        }
        let above = p.actions[0].rect.y as f32 - 1.0;
        assert_eq!(p.action_at(p.actions[0].rect.x as f32, above), None);
        let right = (p.actions[0].rect.x + p.actions[0].rect.w) as f32;
        assert_eq!(p.action_at(right, p.actions[0].rect.y as f32), None);
    }

    #[test]
    fn a_narrow_pane_cuts_the_path_in_the_middle_and_keeps_inside() {
        let narrow = Rect { w: 180, ..CONTENT };
        let failure = gone("/home/me/photos");
        let p = layout(narrow, &failure, &[Action::Up, Action::Retry]);
        for line in p.lines.iter().chain(p.actions.iter().map(|a| &a.label)) {
            assert!(line.rect.x >= narrow.x, "{line:?}");
            assert!(line.rect.x + line.rect.w <= narrow.x + narrow.w, "{line:?}");
        }
        let path = &p.lines[1].text;
        assert!(path.contains('…'), "{path}");
        assert!(path.starts_with('/') && path.ends_with('t'), "{path}");
    }

    #[test]
    fn no_detail_no_small_print_and_no_actions_no_rows() {
        let failure = Unreadable {
            detail: String::new(),
            ..gone("/")
        };
        let p = layout(CONTENT, &failure, &[]);
        assert_eq!(p.lines.len(), 2);
        assert!(p.actions.is_empty());
        assert_eq!(p.action_at(500.0, 300.0), None);
    }
}
