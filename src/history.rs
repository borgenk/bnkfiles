//! Back and forward: where the explorer has been, as a value.
//!
//! ```text
//!   back: [/, /home]   here: /home/me   forward: [/home/me/b]
//!          ◀── back() pops the newest into here, pushing here onto forward
//!                                        forward() is the mirror ──▶
//!   visit(x): here goes onto back, x becomes here, forward is cleared
//! ```
//!
//! The current directory is not stored here: the caller owns it and passes it
//! in, so the history can never disagree with what is on screen. Only a
//! navigation the user chose is a visit; stepping back or forward is not, or
//! back and forward would undo themselves. Nor is the explorer climbing out
//! of a folder that has gone, so the place it climbs to can be the newest
//! one behind it already: a step to where it already is is no step, and is
//! passed over.

use std::path::{Path, PathBuf};

/// How many places each way are remembered. A session that walks further than
/// this forgets its oldest steps rather than growing without bound.
const LIMIT: usize = 100;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct History {
    /// Oldest first, so the newest is the last.
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// Leave `from` for somewhere new. A branch: whatever was ahead is gone,
    /// as in a browser. Going nowhere (to `from` itself) is not a visit.
    pub fn visit(&mut self, from: &Path, to: &Path) {
        if from == to {
            return;
        }
        push_bounded(&mut self.back, from.to_path_buf());
        self.forward.clear();
    }

    /// The place before `here`, which `here` is now ahead of.
    pub fn back(&mut self, here: &Path) -> Option<PathBuf> {
        let to = pop_away(&mut self.back, here)?;
        push_bounded(&mut self.forward, here.to_path_buf());
        Some(to)
    }

    /// The place after `here`, which `here` is now behind.
    pub fn forward(&mut self, here: &Path) -> Option<PathBuf> {
        let to = pop_away(&mut self.forward, here)?;
        push_bounded(&mut self.back, here.to_path_buf());
        Some(to)
    }

    pub fn can_go_back(&self, here: &Path) -> bool {
        self.back.iter().any(|p| p != here)
    }

    pub fn can_go_forward(&self, here: &Path) -> bool {
        self.forward.iter().any(|p| p != here)
    }

    /// How many steps there are each way, for the probe line.
    pub fn depth(&self) -> (usize, usize) {
        (self.back.len(), self.forward.len())
    }
}

/// The newest place on `stack` that is not `here`, dropping those that are.
fn pop_away(stack: &mut Vec<PathBuf>, here: &Path) -> Option<PathBuf> {
    while let Some(to) = stack.pop() {
        if to != here {
            return Some(to);
        }
    }
    None
}

fn push_bounded(stack: &mut Vec<PathBuf>, path: PathBuf) {
    if stack.len() == LIMIT {
        stack.remove(0);
    }
    stack.push(path);
}

#[cfg(test)]
mod tests {
    use crate::history::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// Walk a script of moves from `start`, returning where each one landed:
    /// `v:x` visits x, `c:x` climbs to x without a visit, `b` goes back, `f`
    /// goes forward. A move with nowhere to go stays put and records `-`.
    fn walk(start: &str, moves: &[&str]) -> Vec<String> {
        let mut h = History::new();
        let mut here = p(start);
        let mut out = Vec::new();
        for m in moves {
            let landed = match *m {
                "b" => h.back(&here),
                "f" => h.forward(&here),
                climb if climb.starts_with("c:") => climb.strip_prefix("c:").map(p),
                visit => {
                    let to = p(visit.strip_prefix("v:").expect("a move"));
                    h.visit(&here, &to);
                    Some(to)
                }
            };
            match landed {
                Some(to) => {
                    out.push(to.display().to_string());
                    here = to;
                }
                None => out.push("-".to_string()),
            }
        }
        out
    }

    #[test]
    fn back_and_forward_retrace_the_same_steps() {
        assert_eq!(
            walk("/", &["v:/a", "v:/a/b", "b", "b", "b", "f", "f", "f"]),
            ["/a", "/a/b", "/a", "/", "-", "/a", "/a/b", "-"]
        );
    }

    #[test]
    fn a_visit_after_going_back_drops_what_was_ahead() {
        // Back from /a/b to /a, then somewhere else: /a/b is no longer
        // forward of anything, and back from /c is /a, not /a/b.
        assert_eq!(
            walk(
                "/",
                &["v:/a", "v:/a/b", "b", "v:/c", "f", "b", "b", "f", "f"]
            ),
            ["/a", "/a/b", "/a", "/c", "-", "/a", "/", "/a", "/c"]
        );
    }

    #[test]
    fn going_nowhere_is_not_a_step() {
        // Clicking the breadcrumb segment for the folder already open.
        assert_eq!(walk("/a", &["v:/a", "b"]), ["/a", "-"]);
    }

    #[test]
    fn the_flags_say_whether_a_step_exists() {
        let mut h = History::new();
        let (root, a) = (p("/"), p("/a"));
        assert!(!h.can_go_back(&root) && !h.can_go_forward(&root));
        h.visit(&root, &a);
        assert!(h.can_go_back(&a) && !h.can_go_forward(&a));
        h.back(&a);
        assert!(!h.can_go_back(&root) && h.can_go_forward(&root));
        assert_eq!(h.depth(), (0, 1));
        // The only step ahead is to here: there is none.
        assert!(!h.can_go_forward(&a));
    }

    #[test]
    fn climbing_out_of_a_gone_folder_onto_the_last_place_is_not_a_step_back_to_it() {
        // In /a/b, came from /a; /a/b goes and the explorer climbs to /a.
        // Back goes on to /, rather than reopening /a where it already is.
        assert_eq!(
            walk("/", &["v:/a", "v:/a/b", "c:/a", "b", "f"]),
            ["/a", "/a/b", "/a", "/", "/a"]
        );
        assert_eq!(walk("/a", &["v:/a/b", "c:/a", "b"]), ["/a/b", "/a", "-"]);
    }

    #[test]
    fn a_long_walk_forgets_its_oldest_steps() {
        let mut h = History::new();
        for i in 0..LIMIT + 10 {
            h.visit(&p(&format!("/{i}")), &p(&format!("/{}", i + 1)));
        }
        assert_eq!(h.depth(), (LIMIT, 0));
        let mut here = p(&format!("/{}", LIMIT + 10));
        let mut last = here.clone();
        while let Some(to) = h.back(&here) {
            last = to.clone();
            here = to;
        }
        assert_eq!(last, p("/10"), "the first ten steps are forgotten");
    }
}
