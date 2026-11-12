//! File operations: starting rename, trash and delete, the prompts they
//! raise, and what the job worker reports back.

use std::path::PathBuf;
use std::time::Instant;

use crate::app::State;
use crate::files;
use crate::jobs::{self, ConflictReply, JobEvent, JobKind};
use crate::notice::Level;
use crate::prompt::{ConflictChoice, Prompt};

impl State {
    /// Fold in whatever the job workers have reported.
    ///
    /// A job that ends does not refresh the listing itself: it arms the same
    /// debounce the directory watch uses, so a trash that inotify also notices
    /// costs one rescan rather than two, and a job that changed some *other*
    /// directory still leaves this one correct.
    pub(super) fn handle_jobs(&mut self) {
        self.jobs.drain_wake();
        while let Some((kind, event)) = self.jobs.try_recv() {
            // Any ending takes down a question the job that just ended had
            // asked. Leaving it up would send the user's answer to whichever job
            // started next, which is not the one they were looking at.
            if !matches!(event, JobEvent::Conflict { .. }) {
                self.deferred_conflict = None;
                if matches!(self.prompt, Some(Prompt::Conflict { .. })) {
                    self.prompt = None;
                }
            }
            match event {
                // The worker is blocked until this is answered.
                JobEvent::Conflict { path } if self.prompt.is_some() => {
                    self.deferred_conflict = Some(path);
                }
                JobEvent::Conflict { path } => self.open_prompt(Prompt::conflict(path)),
                JobEvent::Done { items } => {
                    self.notify(
                        Level::Info,
                        format!(
                            "{} {items} item{}",
                            kind.past(),
                            if items == 1 { "" } else { "s" }
                        ),
                    );
                    self.rescan.touch(Instant::now());
                }
                JobEvent::Failed(e) => {
                    self.notify(Level::Error, e);
                    self.rescan.touch(Instant::now());
                }
                JobEvent::Cancelled => {
                    self.notify(Level::Info, format!("{} cancelled", kind.verb()));
                    self.rescan.touch(Instant::now());
                }
            }
            self.dirty = true;
        }
        if self.jobs.took_progress() {
            self.dirty = true;
        }
    }

    /// The paths the user has selected, in the order they are shown.
    pub(super) fn selected_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .sel
            .iter()
            .filter_map(|row| self.row_entry(row).map(|e| e.path.clone()))
            .collect();
        // Acting with nothing selected acts on what the cursor is on, which is
        // what every other file manager does and what a keyboard-driven walk
        // through a directory expects.
        if paths.is_empty() {
            if let Some(entry) = self.lead_entry() {
                paths.push(entry.path.clone());
            }
        }
        paths
    }

    /// Start renaming whatever the cursor is on.
    pub(super) fn start_rename(&mut self) {
        if let Err(e) = self.jobs.permit(JobKind::Rename) {
            self.notify(Level::Error, e.to_string());
            return;
        }
        let Some(entry) = self.lead_entry() else {
            return;
        };
        let prompt = Prompt::rename(entry.path.clone(), &entry.name);
        self.open_prompt(prompt);
    }

    /// Send the selection to the trash.
    pub(super) fn trash_selection(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        self.submit_job(jobs::Request::Trash { paths });
        self.dirty = true;
    }

    /// Ask before removing the selection for good.
    pub(super) fn confirm_delete_selection(&mut self) {
        if let Err(e) = self.jobs.permit(JobKind::Delete) {
            self.notify(Level::Error, e.to_string());
            return;
        }
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        self.open_prompt(Prompt::confirm_delete(paths));
    }

    /// Put a question on screen. A key held down when it appears stops repeating:
    /// an arrow that was walking the list must not go on to walk the answers.
    pub(super) fn open_prompt(&mut self, prompt: Prompt) {
        self.stop_repeat();
        // A question outranks the command list: a job's conflict may arrive
        // while it is up, and the two are never on screen together.
        self.menu = None;
        self.prompt = Some(prompt);
        self.dirty = true;
    }

    /// Hand a request to the job runner, saying why on the notification line if
    /// it is refused.
    pub(super) fn submit_job(&mut self, req: jobs::Request) {
        if let Err(e) = self.jobs.submit(req) {
            self.notify(Level::Error, e.to_string());
        }
    }

    /// Answer the prompt on screen and take it down.
    pub(super) fn answer_prompt(&mut self, accepted: bool) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        // Read the highlighted answer before the prompt is taken apart below.
        let choice = prompt.conflict_choice();
        match prompt {
            Prompt::Rename { path, field } => {
                if accepted && field.is_usable() && field.text() != files::file_name(&path) {
                    self.submit_job(jobs::Request::Rename {
                        path,
                        name: field.text().to_string(),
                    });
                }
            }
            Prompt::Conflict { all, .. } => {
                if !accepted {
                    // Escape at a conflict calls the whole job off rather than
                    // skipping one file: the answer to "I did not mean this" is
                    // to stop, not to carry on through the rest of it.
                    self.jobs.cancel();
                    self.dirty = true;
                    return;
                }
                let reply = match choice {
                    Some(ConflictChoice::Overwrite) => ConflictReply::Overwrite,
                    Some(ConflictChoice::KeepBoth) => ConflictReply::RenameNew,
                    _ => ConflictReply::Skip,
                };
                self.jobs.answer(if all {
                    ConflictReply::ApplyToAll(Box::new(reply))
                } else {
                    reply
                });
            }
            Prompt::ConfirmDelete { paths, yes } => {
                if accepted && yes {
                    self.submit_job(jobs::Request::Delete { paths });
                }
            }
        }
        if let Some(path) = self.deferred_conflict.take() {
            self.open_prompt(Prompt::conflict(path));
        }
        self.dirty = true;
    }
}
