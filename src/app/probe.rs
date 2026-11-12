//! The probe line: the app's state, printed after every frame it presents,
//! when `BNKFILES_PROBE` is set.
//!
//! The UI tests (`tests/ui.rs`) drive the real binary inside a headless
//! compositor. A screenshot tells them what the window looks like; it cannot
//! tell them whether the app has finished reacting to the key they just sent.
//! This line does: a test sends input, then reads lines until one says what it
//! is waiting for.
//!
//! ```text
//!   probe frame=12 mode=grid size=900x700 rows=30 lead=4 selected=1 scan=done
//!         decoding=0 retired=0 index=0 shown=- prompt=0 framed=0 back=0 forward=0
//!         inspector=0 sidebar=1 filter=0 total=30 images=30 places=5
//!         collections=0 sort=name:asc shape=auto:square menu=- focus=entries
//!         unreadable=- notice=- gone=0 dir=/tmp/fixture
//! ```
//!
//! One line, `key=value` pairs separated by spaces. `dir` is last and runs to
//! the end of the line, because a path may hold a space.

use std::io::Write;

use crate::app::{Focus, State, ViewMode};

impl State {
    /// Print the probe line for the frame just presented.
    pub(super) fn report_probe(&self) {
        let mode = match self.mode {
            ViewMode::List => "list",
            ViewMode::Grid => "grid",
            ViewMode::Viewer => "viewer",
        };
        let focus = match self.focus {
            Focus::Entries => "entries",
            Focus::Sidebar => "sidebar",
            Focus::Path => "path",
            Focus::Filter => "filter",
            Focus::Inspector => "inspector",
        };
        let or_dash = |v: Option<usize>| v.map_or_else(|| "-".to_string(), |v| v.to_string());
        let dir = self
            .current_dir
            .as_deref()
            .map(|d| d.to_string_lossy())
            .unwrap_or_default();
        // A reader that has gone away is the test having finished: nothing to
        // report it to, and not a reason to stop the app.
        let _ = writeln!(
            std::io::stdout(),
            "probe frame={} mode={mode} size={}x{} rows={} lead={} selected={} scan={} \
             decoding={} retired={} index={} shown={} prompt={} framed={} back={} forward={} inspector={} sidebar={} filter={} total={} images={} places={} collections={} sort={}:{} shape={}:{} menu={} focus={focus} unreadable={} notice={} gone={} dir={dir}",
            self.presentation.frame_count,
            self.width,
            self.height,
            self.rows.len(),
            or_dash(self.sel.lead()),
            self.sel.count(),
            // A re-read swapped in at the end (a refresh, a new order) is a
            // scan in flight too, though the old listing stays up meanwhile.
            if self.scan_done && self.refresh.is_none() {
                "done"
            } else {
                "busy"
            },
            self.inflight.len(),
            self.presentation.retired_count(),
            self.index,
            or_dash(self.shown),
            u8::from(self.prompt.is_some()),
            u8::from(self.viewer_framed),
            self.history.depth().0,
            self.history.depth().1,
            u8::from(self.regions().inspector.w > 0),
            u8::from(self.regions().sidebar.w > 0),
            u8::from(self.filter.is_active()),
            self.rows_total,
            self.images.len(),
            self.sidebar_rows().len(),
            self.collections.len(),
            self.order.key.key(),
            if self.order.descending { "desc" } else { "asc" },
            // The shape chosen, and what the grid is in now: `-` while auto
            // has not settled.
            self.folder_shape().key(),
            match (self.folder_shape(), self.auto_masonry) {
                (crate::masonry::Shape::Auto, None) => "-",
                _ if self.masonry_now() => "masonry",
                _ => "square",
            },
            // How many commands the list shows, or `-` when it is closed.
            self.menu
                .as_ref()
                .map_or_else(|| "-".to_string(), |m| m.shown().len().to_string()),
            // Why the folder could not be read, or `-` when it could.
            self.unreadable.as_ref().map_or("-", |u| u.reason.key()),
            // The level of the notice up, or `-`.
            self.notice_up().map_or("-", |n| n.level.key()),
            // How many of the sidebar's places are known to be gone.
            self.places
                .iter()
                .filter(|p| self.gone_places.contains(&p.path))
                .count(),
        );
    }
}
