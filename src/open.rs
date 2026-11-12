//! Handing a file to the application the desktop says opens it.
//!
//! Three steps, each testable on its own:
//!
//! 1. **Which application?** `mimeapps.list` names a `.desktop` file per type;
//!    failing that, the `.desktop` files themselves are scanned for one claiming
//!    the type in its `MimeType=`. A type nothing handles is retried as the
//!    types it is a subclass of.
//! 2. **What command?** The entry's `Exec=` is tokenized per the Desktop Entry
//!    spec — quoting, escapes, and the field codes that stand in for the file.
//! 3. **How is it run?** Double-fork with `setsid`, stdio to `/dev/null`. The
//!    grandchild is reparented to init, so nothing we spawn ever becomes a zombie
//!    waiting for an explorer that is busy drawing. An exec that fails is
//!    reported back over a pipe, so "opening x" is never said of a program that
//!    did not start.
//!
//! The command is exec'd directly, never through a shell, so shell
//! metacharacters in a `.desktop` file on disk are inert.

use std::ffi::{c_char, c_int, c_long, c_uint, c_void, CString, OsString};
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::platform::error::{Error, Result};
use crate::platform::ffi::{pipe, PollSet};

/// How much of a `.desktop` file is worth reading. They are a few hundred bytes;
/// anything past this is not a desktop entry and is not going to be parsed into
/// one.
const DESKTOP_FILE_MAX: u64 = 64 * 1024;

/// How much of a shared-mime-info `subclasses` file is worth reading. The real
/// one is a few hundred lines.
const SUBCLASSES_FILE_MAX: u64 = 1024 * 1024;

/// How many parent types a lookup will try. The real hierarchy is a handful of
/// levels deep; the bound is there so a malformed table cannot make an open walk
/// the whole database.
const ANCESTORS_MAX: usize = 16;

/// What a `Terminal=true` entry starts through. It opens the user's preferred
/// terminal running the command line after it.
const TERMINAL_RUNNER: &str = "xdg-terminal-exec";

/// How long a spawn waits for the child to reach its exec. The explorer is not
/// drawing while it waits, so an exec that takes longer (a `PATH` entry on a
/// mount that stopped answering) counts as started.
const EXEC_REPORT_WAIT: Duration = Duration::from_millis(500);

/// Open `path`, which has been sniffed as `mime`, in whatever handles that type.
pub fn open_path(path: &Path, mime: &str) -> Result<()> {
    let dirs = Dirs::from_env();
    let entry = resolve_handler(&dirs, mime)
        .ok_or_else(|| Error::msg(format!("no application is registered for {mime}")))?;
    let argv = command_for(&entry, path, &dirs)?;
    spawn_detached(&argv)
}

/// The command line that opens `path` with `entry`.
fn command_for(entry: &Entry, path: &Path, dirs: &Dirs) -> Result<Vec<OsString>> {
    // The application starts with its own idea of the working directory and a
    // `file://` URL has no relative form, so the path is anchored first.
    // Lexically, not by resolving symlinks: the application should see the name
    // the user opened.
    let path = std::path::absolute(path)?;
    let mut argv = exec_argv(&entry.exec, &path);
    if argv.is_empty() {
        return Err(Error::msg(format!("{} has an empty Exec line", entry.id)));
    }
    if entry.terminal {
        // Run bare, a terminal program would start with its stdio on /dev/null
        // and no window at all, which looks exactly like nothing happening.
        if !program_found(TERMINAL_RUNNER, &dirs.bin) {
            return Err(Error::msg(format!(
                "{} runs in a terminal, and {TERMINAL_RUNNER} is not installed",
                entry.id
            )));
        }
        argv.insert(0, OsString::from(TERMINAL_RUNNER));
    }
    Ok(argv)
}

/// A `.desktop` file that was found, read, and can be launched.
#[derive(Debug)]
struct Entry {
    /// The desktop id, e.g. `org.gnome.Loupe.desktop`, for error messages.
    id: String,
    /// The `Exec` value with its string escapes already resolved.
    exec: String,
    /// `Terminal=true`: the program needs a terminal to run in.
    terminal: bool,
}

impl Entry {
    /// The launchable application `text` describes, if it describes one.
    ///
    /// Not every file that exists is a handler: a `Hidden=true` entry is the
    /// spec's way of deleting an application, a `TryExec` naming a program that
    /// is gone marks a stale entry left behind by an uninstall, and a `Link` or
    /// `Directory` entry is not a program at all.
    fn parse(id: &str, text: &str, dirs: &Dirs) -> Option<Self> {
        if desktop_field(text, "Type").as_deref() != Some("Application")
            || desktop_field(text, "Hidden").as_deref() == Some("true")
        {
            return None;
        }
        if let Some(try_exec) = desktop_field(text, "TryExec") {
            if !program_found(&try_exec, &dirs.bin) {
                return None;
            }
        }
        Some(Self {
            id: id.to_string(),
            exec: desktop_field(text, "Exec")?,
            terminal: desktop_field(text, "Terminal").as_deref() == Some("true"),
        })
    }
}

/// Where to look for applications: the data directories, the config directory
/// whose `mimeapps.list` records the user's own choices, and the directories
/// programs are run from.
///
/// Passed rather than read from the environment at the point of use, so the
/// resolution can be exercised against a directory tree a test built instead of
/// against whatever this machine happens to have installed.
struct Dirs {
    data: Vec<PathBuf>,
    config: Option<PathBuf>,
    /// The `PATH` directories, for `TryExec` and the terminal runner.
    bin: Vec<PathBuf>,
}

impl Dirs {
    /// What the environment says, per the XDG basedir spec.
    fn from_env() -> Self {
        Self {
            data: data_dirs(),
            config: config_home(),
            bin: bin_dirs(),
        }
    }

    /// The association files, in the order they win: the user's own choices
    /// first, then the system-wide defaults beside the applications themselves.
    fn mimeapps(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(config) = self.config.as_ref() {
            out.push(config.join("mimeapps.list"));
        }
        for dir in &self.data {
            out.push(dir.join("applications").join("mimeapps.list"));
            out.push(dir.join("applications").join("defaults.list"));
        }
        out
    }
}

/// Find the application that opens `mime`.
///
/// The type itself first. Only when nothing handles it are its parent types
/// tried, nearest first: a Rust source file with no handler of its own is still
/// text, and whatever opens `text/plain` opens it.
fn resolve_handler(dirs: &Dirs, mime: &str) -> Option<Entry> {
    resolve_exact(dirs, mime).or_else(|| {
        ancestors(dirs, mime)
            .iter()
            .find_map(|parent| resolve_exact(dirs, parent))
    })
}

/// Find the application registered for exactly `mime`.
///
/// The association files come first, most specific first: the user's own choices,
/// then the system defaults. Only when none of them names a handler that can
/// actually be launched does this fall back to reading every `.desktop` file
/// looking for one that claims the type.
fn resolve_exact(dirs: &Dirs, mime: &str) -> Option<Entry> {
    for list in dirs.mimeapps() {
        let Some(text) = read_capped(&list, DESKTOP_FILE_MAX) else {
            continue;
        };
        for id in associations(&text, mime) {
            if let Some(entry) = read_desktop(dirs, &id) {
                return Some(entry);
            }
        }
    }
    scan_for_handler(dirs, mime)
}

/// The types `mime` is a subclass of, nearest first, parents of parents included.
///
/// Read from shared-mime-info's `subclasses` table (`child parent` per line) in
/// each data directory. That is the one file of the MIME database simple enough
/// to read without parsing the database. On top of it sits the rule the spec
/// states outright: every `text/*` type is a `text/plain`.
fn ancestors(dirs: &Dirs, mime: &str) -> Vec<String> {
    let tables: Vec<String> = dirs
        .data
        .iter()
        .filter_map(|dir| read_capped(&dir.join("mime").join("subclasses"), SUBCLASSES_FILE_MAX))
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut current = mime.to_string();
    let mut next = 0;
    loop {
        let mut parents: Vec<&str> = tables
            .iter()
            .flat_map(|table| table.lines())
            .filter_map(|line| {
                let (child, parent) = line.split_once(' ')?;
                (child == current).then_some(parent.trim())
            })
            .collect();
        if current.starts_with("text/") {
            parents.push("text/plain");
        }
        for parent in parents {
            let seen = parent == mime || out.iter().any(|known| known == parent);
            if !seen && out.len() < ANCESTORS_MAX {
                out.push(parent.to_string());
            }
        }
        // Breadth first: every parent is tried before any grandparent.
        match out.get(next) {
            Some(parent) => current = parent.clone(),
            None => return out,
        }
        next += 1;
    }
}

/// The desktop ids `text` associates with `mime`, in order: `[Default
/// Applications]` first, then `[Added Associations]`.
///
/// The value is a semicolon-separated list, and a `.desktop` file named there may
/// well not be installed, so every candidate is returned and the caller takes the
/// first that resolves.
fn associations(text: &str, mime: &str) -> Vec<String> {
    let mut defaults = Vec::new();
    let mut added = Vec::new();
    let mut section = "";
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != mime {
            continue;
        }
        let ids = value
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        match section {
            "[Default Applications]" => defaults.extend(ids),
            "[Added Associations]" => added.extend(ids),
            // [Removed Associations] and anything else is not a handler.
            _ => {}
        }
    }
    defaults.extend(added);
    defaults
}

/// The launchable application with this desktop id, if there is one.
fn read_desktop(dirs: &Dirs, id: &str) -> Option<Entry> {
    Entry::parse(id, &desktop_text(dirs, id)?, dirs)
}

/// The text of the `.desktop` file with this id.
///
/// The first data directory that has the file decides, and the same id further
/// down is never consulted. That is what lets a user's `Hidden=true` copy in
/// their own directory delete a system-wide application: the copy is found, it
/// is not launchable, and the search for this id ends there.
fn desktop_text(dirs: &Dirs, id: &str) -> Option<String> {
    // An id may encode a subdirectory as a dash (`foo-bar.desktop` can live at
    // `foo/bar.desktop`), but the plain path is what nearly every entry uses and
    // is what is tried here.
    dirs.data
        .iter()
        .find_map(|dir| read_capped(&dir.join("applications").join(id), DESKTOP_FILE_MAX))
}

/// Read every installed `.desktop` file and take the first that claims `mime`.
///
/// The slow path, reached only when the association files named nothing usable.
/// It is a few hundred small reads, and it happens on an explicit open rather
/// than while drawing.
fn scan_for_handler(dirs: &Dirs, mime: &str) -> Option<Entry> {
    for dir in &dirs.data {
        let apps = dir.join("applications");
        let Ok(entries) = std::fs::read_dir(&apps) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            // Looked up by id rather than read from where it was found, so a
            // file a higher-precedence directory overrides is judged by the
            // override.
            let id = crate::files::file_name(&path);
            let Some(text) = desktop_text(dirs, &id) else {
                continue;
            };
            if !claims_mime(&text, mime) {
                continue;
            }
            if let Some(entry) = Entry::parse(&id, &text, dirs) {
                return Some(entry);
            }
        }
    }
    None
}

/// Whether this entry's `MimeType=` lists `mime`, and it is an application a
/// person is meant to be offered.
fn claims_mime(text: &str, mime: &str) -> bool {
    if desktop_field(text, "NoDisplay").as_deref() == Some("true") {
        return false;
    }
    desktop_field(text, "MimeType")
        .map(|types| types.split(';').map(str::trim).any(|t| t == mime))
        .unwrap_or(false)
}

/// One key's value from the `[Desktop Entry]` group, string escapes resolved.
///
/// Only that group: an action group further down the file has its own `Exec=`,
/// and taking that one would launch the wrong thing. Localized keys (`Name[de]`)
/// are skipped, since the plain key is what everything here wants.
fn desktop_field(text: &str, key: &str) -> Option<String> {
    let mut in_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        if let Some((k, value)) = line.split_once('=') {
            if k.trim() == key {
                return Some(unescape(value.trim()));
            }
        }
    }
    None
}

/// Resolve the escape sequences of a desktop-entry string value: `\s`, `\n`,
/// `\t`, `\r` and `\\`. Any other backslash is kept as it stands.
///
/// The spec applies these before the `Exec` quoting rules, so `\\` has to
/// collapse here: leave it, and the tokenizer reads two backslashes where the
/// file meant one.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Whether `program` can be run: a path is checked as it stands, a bare name is
/// looked for in the `bin` directories, as exec would.
fn program_found(program: &str, bin: &[PathBuf]) -> bool {
    if program.contains('/') {
        return is_executable(Path::new(program));
    }
    bin.iter().any(|dir| is_executable(&dir.join(program)))
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Tokenize an `Exec` value per the Desktop Entry spec, substituting `path` for
/// the field codes that stand for a file.
///
/// `exec` is the value after its string escapes were resolved, which is what
/// [`desktop_field`] hands out. Double quotes group an argument, and inside them
/// a backslash escapes the four reserved characters. `%%` is a literal percent.
/// `%f`/`%F` become the file's path and `%u`/`%U` its `file://` URL; every other
/// code (`%i`, `%c`, `%k`, and the deprecated ones) expands to nothing. Only the
/// code letter is consumed, so a stray `%` before a space cannot swallow the
/// separator and glue two arguments into one.
///
/// An entry that names no field code at all still gets the file appended: an
/// application registered for a MIME type that then receives no file to open is
/// useless, and a malformed `Exec` is common enough to be worth surviving.
///
/// The arguments are byte strings, not `String`s: a file name is whatever bytes
/// the filesystem holds, and one that is not UTF-8 has to reach the application
/// as it is or it names a file that does not exist.
pub fn exec_argv(exec: &str, path: &Path) -> Vec<OsString> {
    let file = path.as_os_str().as_bytes();
    let url = file_url(path);
    let mut argv: Vec<OsString> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut has_arg = false;
    let mut took_file = false;
    let mut chars = exec.chars().peekable();
    let push = |current: &mut Vec<u8>, ch: char| {
        current.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
    };

    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\t' => {
                if has_arg {
                    argv.push(OsString::from_vec(std::mem::take(&mut current)));
                    has_arg = false;
                }
            }
            '"' => {
                has_arg = true;
                while let Some(quoted) = chars.next() {
                    match quoted {
                        '"' => break,
                        '\\' => match chars.peek() {
                            Some(&next @ ('"' | '`' | '$' | '\\')) => {
                                push(&mut current, next);
                                chars.next();
                            }
                            _ => current.push(b'\\'),
                        },
                        other => push(&mut current, other),
                    }
                }
            }
            '%' => match chars.peek().copied() {
                Some('%') => {
                    chars.next();
                    current.push(b'%');
                    has_arg = true;
                }
                Some('f' | 'F') => {
                    chars.next();
                    current.extend_from_slice(file);
                    has_arg = true;
                    took_file = true;
                }
                Some('u' | 'U') => {
                    chars.next();
                    current.extend_from_slice(url.as_bytes());
                    has_arg = true;
                    took_file = true;
                }
                // Every other code expands to nothing.
                Some(next) if next != ' ' && next != '\t' => {
                    chars.next();
                }
                _ => {}
            },
            other => {
                push(&mut current, other);
                has_arg = true;
            }
        }
    }
    if has_arg {
        argv.push(OsString::from_vec(current));
    }
    if !took_file && !argv.is_empty() {
        argv.push(path.as_os_str().to_os_string());
    }
    argv
}

/// A path as a `file://` URL, percent-encoding what a URL may not carry raw.
/// The path has to be absolute for the result to mean anything.
pub fn file_url(path: &Path) -> String {
    let mut out = String::from("file://");
    out.push_str(&percent_encode_path(path));
    out
}

/// A path with everything a URL may not carry raw percent-encoded, and the
/// separators left as themselves.
///
/// Shared with the trash, whose `.trashinfo` files store the original location
/// "with characters escaped as in URLs" but without a scheme in front. Encoding
/// the separators would break restoring, since the structure of the path is
/// exactly what a restore has to read back.
///
/// The path's own bytes are encoded, whatever they are, so a name that is not
/// UTF-8 decodes back to the same name.
pub fn percent_encode_path(path: &Path) -> String {
    let mut out = String::new();
    for &byte in path.as_os_str().as_bytes() {
        match byte {
            // Unreserved, plus the separators a path needs to keep as themselves.
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ---- spawning ------------------------------------------------------------

// Declared here rather than in `platform/ffi.rs`: spawning a handler is this
// module's business, not the platform layer's, so the calls live with the one
// module that makes them.
extern "C" {
    fn fork() -> c_int;
    fn setsid() -> c_int;
    fn execvp(file: *const c_char, argv: *const *const c_char) -> c_int;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    fn signal(signum: c_int, handler: usize) -> usize;
    fn sigprocmask(how: c_int, set: *const SignalSet, old: *mut SignalSet) -> c_int;
    fn syscall(number: c_long, ...) -> c_long;
    fn __errno_location() -> *mut c_int;
    fn _exit(status: c_int) -> !;
}

const O_RDWR: c_int = 2;
const SIG_DFL: usize = 0;
const SIG_SETMASK: c_int = 2;
/// The highest signal number Linux has; the real-time signals end here.
const SIGNAL_MAX: c_int = 64;
/// `close_range` by number: the libc wrapper is newer than the oldest libc this
/// should link against, and the number is the same on every architecture.
const SYS_CLOSE_RANGE: c_long = 436;
const CLOSE_RANGE_CLOEXEC: c_long = 1 << 2;

/// libc's `sigset_t`, which is 1024 bits whatever the kernel's is. All zero is
/// the empty set.
#[repr(C)]
struct SignalSet([u64; 16]);

/// Start `argv` detached from this process, returning once its exec has gone
/// through or with the reason it did not.
///
/// Double fork: the child calls `setsid` (its own session, so it does not die
/// with our terminal) and forks again; the grandchild execs, and the child exits
/// at once. The grandchild is therefore an orphan, reparented to init, and can
/// never be a zombie we are expected to reap — which matters because an explorer
/// spends its life in `poll`, not in `wait`. The intermediate child *is* ours,
/// and is reaped here immediately.
///
/// ```text
///   explorer ──fork──▶ child ──fork──▶ grandchild ──exec──▶ application
///      │                 │ _exit(0)        │
///      │◀── waitpid ─────┘                 │ exec failed: errno ─┐
///      │◀── report pipe (close-on-exec: EOF once the exec went through) ─┘
/// ```
fn spawn_detached(argv: &[OsString]) -> Result<()> {
    let args: Vec<CString> = argv
        .iter()
        .map(|a| CString::new(a.as_bytes()))
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| Error::msg("an argument contains an interior NUL byte"))?;
    let (Some(program), Some(name)) = (args.first(), argv.first()) else {
        return Err(Error::msg("there is no command to run"));
    };
    let mut pointers: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();
    pointers.push(std::ptr::null());
    let devnull = c"/dev/null";
    let no_signals = SignalSet([0; 16]);

    // Both ends are close-on-exec. An exec that goes through closes the
    // grandchild's write end and the read below sees end of file; one that fails
    // leaves the end open just long enough to send its errno.
    let (report_read, report_write) = pipe()?;
    let report_fd = report_write.as_raw_fd();

    // SAFETY: fork in a process that has other threads (the scan and decode
    // workers) is only safe if the child does async-signal-safe work and nothing
    // else before exec, which is all the branch below does: setsid, fork,
    // signal, sigprocmask, open, dup2, close, close_range, execvp, write, _exit.
    // Nothing allocates, takes a lock, or touches Rust runtime state; every
    // string and buffer it passes was built before the fork.
    let child = unsafe { fork() };
    if child < 0 {
        return Err(Error::msg(format!(
            "could not fork: {}",
            std::io::Error::last_os_error()
        )));
    }
    if child == 0 {
        // SAFETY: as above; this branch runs in the forked child only. Every
        // pointer is to memory that outlives the fork: the argv and path strings
        // above, and locals of this frame.
        unsafe {
            setsid();
            let grandchild = fork();
            if grandchild < 0 {
                // Nothing will exec. The exit status tells the parent which of
                // the two failures the errno in the pipe belongs to.
                report_errno(report_fd);
                _exit(1);
            }
            if grandchild > 0 {
                // The intermediate child's whole job is to orphan the grandchild.
                _exit(0);
            }
            // exec keeps an ignored signal ignored and keeps the mask. The Rust
            // runtime ignores SIGPIPE, and whatever started the explorer may
            // have ignored or blocked more (nohup ignores SIGHUP). So the
            // application gets every signal back at its default with nothing
            // blocked. SIGKILL, SIGSTOP and libc's internal signals refuse the
            // change, which is harmless.
            for signum in 1..=SIGNAL_MAX {
                signal(signum, SIG_DFL);
            }
            sigprocmask(SIG_SETMASK, &no_signals, std::ptr::null_mut());
            let null = open(devnull.as_ptr(), O_RDWR);
            if null >= 0 {
                dup2(null, 0);
                dup2(null, 1);
                dup2(null, 2);
                if null > 2 {
                    close(null);
                }
            }
            // Only stdio goes to the application. Our own descriptors are
            // close-on-exec, but one inherited from whatever started us, or
            // opened by a library, may not be. Marking them rather than closing
            // them keeps the report pipe open until the exec. A kernel too old
            // for the call leaves things as they were.
            syscall(
                SYS_CLOSE_RANGE,
                3 as c_long,
                c_uint::MAX as c_long,
                CLOSE_RANGE_CLOEXEC,
            );
            execvp(program.as_ptr(), pointers.as_ptr());
            // execvp returns only on failure; 127 is the shell's convention for
            // a command that could not be run.
            report_errno(report_fd);
            _exit(127);
        }
    }
    // Ours has to close first, or the pipe never reaches end of file.
    drop(report_write);

    // Reap the intermediate child, which exits immediately. Without this it would
    // sit as a zombie for the life of the process — the very thing the double
    // fork exists to prevent.
    let mut status: c_int = 0;
    loop {
        // SAFETY: a plain syscall with one out-pointer to a live local.
        let reaped = unsafe { waitpid(child, &mut status, 0) };
        let interrupted =
            reaped < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted;
        if !interrupted {
            break;
        }
    }

    let errno = exec_report(report_read, EXEC_REPORT_WAIT);
    if status != 0 {
        let reason = errno.map_or_else(
            || String::from("the helper process failed"),
            |errno| std::io::Error::from_raw_os_error(errno).to_string(),
        );
        return Err(Error::msg(format!("could not fork: {reason}")));
    }
    match errno {
        Some(errno) => Err(Error::msg(format!(
            "could not run {}: {}",
            name.to_string_lossy(),
            std::io::Error::from_raw_os_error(errno)
        ))),
        None => Ok(()),
    }
}

/// Send the calling thread's errno down the report pipe.
///
/// # Safety
///
/// Called between fork and exec, so it may only make async-signal-safe calls.
/// `fd` must be the open write end of the report pipe.
unsafe fn report_errno(fd: c_int) {
    // SAFETY: __errno_location returns this thread's errno slot, valid for as
    // long as the thread runs, and the write reads four bytes of a live local.
    // Four bytes to a pipe arrive whole or not at all.
    unsafe {
        let bytes = (*__errno_location()).to_ne_bytes();
        write(fd, bytes.as_ptr().cast::<c_void>(), bytes.len());
    }
}

/// What came back through the report pipe: the errno of a fork or exec that
/// failed, or `None` once the exec went through and closed the other end.
/// Silence past `wait` also counts as started.
fn exec_report(read_end: OwnedFd, wait: Duration) -> Option<c_int> {
    let mut set = PollSet::new();
    let slot = set.add(read_end.as_raw_fd());
    set.wait(Some(wait)).ok()?;
    if !set.readable(slot) {
        return None;
    }
    let mut errno = [0u8; 4];
    match std::fs::File::from(read_end).read(&mut errno) {
        Ok(n) if n == errno.len() => Some(c_int::from_ne_bytes(errno)),
        _ => None,
    }
}

// ---- XDG directories -----------------------------------------------------

fn config_home() -> Option<PathBuf> {
    absolute_env("XDG_CONFIG_HOME").or_else(|| home().map(|h| h.join(".config")))
}

/// The data directories, most specific first: the user's own, then the system's.
fn data_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = absolute_env("XDG_DATA_HOME") {
        out.push(dir);
    } else if let Some(home) = home() {
        out.push(home.join(".local").join("share"));
    }
    match std::env::var_os("XDG_DATA_DIRS") {
        Some(dirs) if !dirs.is_empty() => out.extend(
            std::env::split_paths(&dirs).filter(|p| p.is_absolute() && !p.as_os_str().is_empty()),
        ),
        // The default the spec names when the variable is unset or empty.
        _ => out.extend([
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ]),
    }
    out
}

/// The directories a bare program name is looked up in.
fn bin_dirs() -> Vec<PathBuf> {
    match std::env::var_os("PATH") {
        Some(path) => std::env::split_paths(&path)
            .filter(|p| !p.as_os_str().is_empty())
            .collect(),
        // What exec searches when the variable is unset.
        None => vec![PathBuf::from("/bin"), PathBuf::from("/usr/bin")],
    }
}

/// An environment variable as an absolute path. A relative one is ignored, which
/// is what the XDG spec says to do with it.
fn absolute_env(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(name)?);
    path.is_absolute().then_some(path)
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Read a text file, refusing anything larger than `max` bytes: past that it is
/// not the small file it is supposed to be.
fn read_capped(path: &Path, max: u64) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > max {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::fd::FromRawFd;
    use std::time::Instant;

    use crate::open::*;

    const MIMEAPPS: &str = "\
[Added Associations]
image/png=gimp.desktop;inkscape.desktop;
text/plain=gedit.desktop;

[Default Applications]
image/png=org.gnome.Loupe.desktop;
application/pdf=org.gnome.Evince.desktop;
text/plain=nvim.desktop;

[Removed Associations]
image/png=eog.desktop;
";

    #[test]
    fn defaults_outrank_added_associations() {
        // The default is what the user chose; the added ones are what can also
        // open it. Both are offered, in that order, because a default naming an
        // application that is not installed must not dead-end the open.
        assert_eq!(
            associations(MIMEAPPS, "image/png"),
            vec![
                "org.gnome.Loupe.desktop",
                "gimp.desktop",
                "inkscape.desktop"
            ]
        );
        assert_eq!(
            associations(MIMEAPPS, "text/plain"),
            vec!["nvim.desktop", "gedit.desktop"]
        );
    }

    #[test]
    fn a_removed_association_is_not_a_handler() {
        // eog appears only under [Removed Associations], so it must not be
        // offered for image/png at all.
        assert!(!associations(MIMEAPPS, "image/png").contains(&"eog.desktop".to_string()));
        // And a type nobody claims has no handlers rather than a wrong one.
        assert!(associations(MIMEAPPS, "video/mp4").is_empty());
    }

    const ENTRY: &str = "\
[Desktop Entry]
Type=Application
Name=Loupe
Name[de]=Lupe
Exec=loupe %U
MimeType=image/png;image/jpeg;

[Desktop Action new-window]
Name=New Window
Exec=loupe --new-window
";

    #[test]
    fn only_the_desktop_entry_groups_fields_are_read() {
        // The action group further down has its own Exec; taking it would launch
        // the wrong command.
        assert_eq!(desktop_field(ENTRY, "Exec").as_deref(), Some("loupe %U"));
        assert_eq!(desktop_field(ENTRY, "Name").as_deref(), Some("Loupe"));
        assert_eq!(desktop_field(ENTRY, "Nope"), None);
    }

    #[test]
    fn string_escapes_resolve_before_the_exec_quoting_rules() {
        // The spec's order: the value is unescaped as a string first, and only
        // then tokenized. So a quoted literal backslash is written with four in
        // the file, and \s is a space that separates arguments like any other.
        let text = "[Desktop Entry]\nExec=app \"a\\\\\\\\b\" one\\stwo \"tab\\there\" %f\n";
        let exec = desktop_field(text, "Exec").expect("Exec");
        assert_eq!(exec, "app \"a\\\\b\" one two \"tab\there\" %f");
        assert_eq!(
            exec_argv(&exec, Path::new("/x.png")),
            vec!["app", r"a\b", "one", "two", "tab\there", "/x.png"]
        );
        // A backslash that starts no string escape is left for the tokenizer.
        assert_eq!(unescape(r#"a\"b\"#), r#"a\"b\"#);
        assert_eq!(unescape(r"\n\r\q"), "\n\r\\q");
    }

    #[test]
    fn an_entry_claims_the_types_it_lists_and_no_others() {
        assert!(claims_mime(ENTRY, "image/png"));
        assert!(claims_mime(ENTRY, "image/jpeg"));
        assert!(!claims_mime(ENTRY, "image/gif"));
        // A prefix of a listed type is not that type.
        assert!(!claims_mime(ENTRY, "image/p"));
        // Entries the desktop hides are not offered. The key has to go *in* the
        // [Desktop Entry] group to count, which is the point of scoping the
        // lookup: appended at the end of the file it lands in the action group
        // and means nothing.
        let hidden = ENTRY.replace("[Desktop Entry]\n", "[Desktop Entry]\nNoDisplay=true\n");
        assert!(!claims_mime(&hidden, "image/png"));
        let appended = format!("{ENTRY}NoDisplay=true\n");
        assert!(
            claims_mime(&appended, "image/png"),
            "a key in a later group must not be read as the entry's"
        );
    }

    fn argv(exec: &str) -> Vec<OsString> {
        exec_argv(exec, Path::new("/home/me/holiday photo.png"))
    }

    #[test]
    fn exec_substitutes_the_file_for_the_field_codes() {
        assert_eq!(
            argv("loupe %f"),
            vec!["loupe", "/home/me/holiday photo.png"]
        );
        assert_eq!(
            argv("loupe %F"),
            vec!["loupe", "/home/me/holiday photo.png"]
        );
        // The URL forms get a percent-encoded file:// URL.
        assert_eq!(
            argv("firefox %u"),
            vec!["firefox", "file:///home/me/holiday%20photo.png"]
        );
    }

    #[test]
    fn a_path_with_spaces_stays_one_argument() {
        // The substitution happens after tokenizing, so a space in the file's own
        // name can never split it into two arguments. Passing it through a shell
        // is exactly the bug this avoids.
        let args = argv("loupe %f");
        assert_eq!(args.len(), 2);
        assert!(args[1].to_string_lossy().contains(' '));
    }

    #[test]
    fn the_other_field_codes_expand_to_nothing() {
        // %i, %c, %k and the deprecated codes have no value here, and dropping
        // the whole token rather than leaving "%i" behind is what the spec says.
        assert_eq!(
            argv("app %i %c %k %f"),
            vec!["app", "/home/me/holiday photo.png"]
        );
        // A literal percent survives as itself.
        assert_eq!(
            argv("app 100%% %f"),
            vec!["app", "100%", "/home/me/holiday photo.png"]
        );
        // A lone % before a separator must not swallow it and glue two arguments.
        assert_eq!(
            argv("app % two %f"),
            vec!["app", "two", "/home/me/holiday photo.png"]
        );
    }

    #[test]
    fn quoting_follows_the_spec() {
        assert_eq!(
            exec_argv(r#""/opt/my app/bin" --flag %f"#, Path::new("/x.png")),
            vec!["/opt/my app/bin", "--flag", "/x.png"]
        );
        // Inside quotes, a backslash escapes the four reserved characters and is
        // otherwise a literal backslash.
        assert_eq!(
            exec_argv(r#"app "a\"b" "c\\d" "e\nf" %f"#, Path::new("/x.png")),
            vec!["app", r#"a"b"#, r"c\d", r"e\nf", "/x.png"]
        );
    }

    #[test]
    fn an_exec_that_names_no_file_still_gets_one() {
        // A malformed entry registered for a MIME type but with no field code
        // would otherwise open the application with nothing in it.
        assert_eq!(argv("loupe"), vec!["loupe", "/home/me/holiday photo.png"]);
        // But an empty Exec yields no command at all, rather than a bare path
        // that would be exec'd as if it were a program.
        assert!(argv("").is_empty());
        assert!(argv("   ").is_empty());
    }

    #[test]
    fn a_file_name_that_is_not_utf8_reaches_the_command_byte_for_byte() {
        // Latin-1 "café": 0xE9 on its own is not UTF-8. Replacing it with U+FFFD
        // would hand the application the name of a file that does not exist.
        let path = Path::new(OsStr::from_bytes(b"/home/me/caf\xe9.txt"));
        assert_eq!(
            exec_argv("app --open %f", path),
            vec![
                OsStr::new("app"),
                OsStr::new("--open"),
                OsStr::from_bytes(b"/home/me/caf\xe9.txt")
            ]
        );
        // A URL carries the byte itself, percent-encoded.
        assert_eq!(
            exec_argv("app %u", path),
            vec!["app", "file:///home/me/caf%E9.txt"]
        );
        // And with no field code the appended path is the same bytes.
        assert_eq!(
            exec_argv("app", path).last().map(|a| a.as_bytes()),
            Some(&b"/home/me/caf\xe9.txt"[..])
        );
    }

    #[test]
    fn file_urls_encode_what_a_url_cannot_carry_raw() {
        assert_eq!(file_url(Path::new("/a/b.png")), "file:///a/b.png");
        assert_eq!(file_url(Path::new("/a b/c.png")), "file:///a%20b/c.png");
        assert_eq!(
            file_url(Path::new("/a#b?c/d.png")),
            "file:///a%23b%3Fc/d.png"
        );
        // Slashes stay slashes, or it would not be a path any more.
        assert_eq!(file_url(Path::new("/x/y/z")), "file:///x/y/z");
        // Non-ASCII is encoded byte by byte, as UTF-8.
        assert_eq!(file_url(Path::new("/caf\u{e9}")), "file:///caf%C3%A9");
    }

    /// An empty directory of this test's own.
    fn scratch(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "bnkfiles_open_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch dir");
        base
    }

    /// Run a shell script detached, its `$0` being `out` and `extra` its
    /// positional parameters.
    fn spawn_sh(script: &str, out: &Path, extra: &[&OsStr]) {
        let mut argv = vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(script),
            out.as_os_str().to_os_string(),
        ];
        argv.extend(extra.iter().map(|a| a.to_os_string()));
        spawn_detached(&argv).expect("spawn /bin/sh");
    }

    /// What a detached script wrote to `path`, once it has written a whole line.
    /// The script outlives the spawn call, so the file has to be waited for.
    fn written_line(path: &Path) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(bytes) = std::fs::read(path) {
                if bytes.ends_with(b"\n") {
                    return bytes;
                }
            }
            assert!(
                Instant::now() < deadline,
                "the spawned script never wrote {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_program_that_cannot_be_executed_is_an_error_naming_it() {
        // The exec fails in the grandchild, after both forks succeeded. It still
        // has to come back as an error, or the status bar says "opening x" and
        // nothing ever appears.
        let missing = "/nonexistent/command/xyzzy";
        let err = spawn_detached(&[OsString::from(missing)]).expect_err("exec must fail");
        assert!(
            err.to_string().contains(missing),
            "the error does not name the program: {err}"
        );
        // A bare name that is on no PATH directory fails the same way.
        let err = spawn_detached(&[OsString::from("bnkfiles-test-no-such-program")])
            .expect_err("exec must fail");
        assert!(err.to_string().contains("bnkfiles-test-no-such-program"));
        // And a program that is there is not an error.
        spawn_detached(&[OsString::from("/bin/true")]).expect("spawn /bin/true");
    }

    #[test]
    fn an_empty_command_is_an_error_not_a_panic() {
        assert!(spawn_detached(&[]).is_err());
    }

    #[test]
    fn a_spawned_command_is_not_our_child() {
        // The double fork's whole point: a process that is not our child can
        // never be a zombie we are expected to reap. Spawned with one fork, the
        // script's parent would be this process.
        let base = scratch("ppid");
        let out = base.join("ppid");
        spawn_sh(r#"echo "$PPID" > "$0""#, &out, &[]);
        let ppid = String::from_utf8(written_line(&out)).expect("a pid");
        assert_ne!(ppid.trim(), std::process::id().to_string());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// The signals a process ignores, as the kernel reports them: bit `n - 1` is
    /// signal `n`.
    fn ignored_signals(status: &str) -> u64 {
        let mask = status
            .lines()
            .find_map(|line| line.strip_prefix("SigIgn:"))
            .expect("a SigIgn line");
        u64::from_str_radix(mask.trim(), 16).expect("a hex mask")
    }

    #[test]
    fn a_spawned_command_gets_its_signals_back_at_their_defaults() {
        const SIGPIPE_BIT: u64 = 1 << 12;
        // The Rust runtime ignores SIGPIPE in this very process, and exec hands
        // an ignored signal on. Without the reset, a pipeline in a spawned
        // terminal never dies of a closed pipe.
        let own = std::fs::read_to_string("/proc/self/status").expect("own status");
        assert_ne!(
            ignored_signals(&own) & SIGPIPE_BIT,
            0,
            "this process was expected to ignore SIGPIPE"
        );
        let base = scratch("signals");
        let out = base.join("status");
        spawn_sh(r#"exec > "$0"; grep SigIgn: /proc/self/status"#, &out, &[]);
        let status = String::from_utf8(written_line(&out)).expect("status text");
        assert_eq!(ignored_signals(&status) & SIGPIPE_BIT, 0, "{status}");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_descriptor_without_close_on_exec_does_not_reach_the_command() {
        extern "C" {
            fn dup(fd: c_int) -> c_int;
        }
        // dup() is the plain way to get a descriptor that survives exec, which
        // is what one inherited from a parent or opened by a library looks like.
        // SAFETY: duplicates this process's stderr; the result is checked and
        // then owned by the OwnedFd, which closes it.
        let leaked = unsafe {
            let fd = dup(2);
            assert!(fd > 2, "dup failed");
            OwnedFd::from_raw_fd(fd)
        };
        let base = scratch("fds");
        let out = base.join("fd");
        let number = leaked.as_raw_fd().to_string();
        // `exec >` rather than a redirect on the test itself: the shell parks
        // the saved stdout on a spare descriptor for the length of a redirected
        // command, and that could land on the number being asked about.
        spawn_sh(
            r#"exec > "$0"; if [ -e "/proc/self/fd/$1" ]; then echo open; else echo closed; fi"#,
            &out,
            &[OsStr::new(&number)],
        );
        assert_eq!(written_line(&out), b"closed\n");
        drop(leaked);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn an_argument_that_is_not_utf8_is_passed_through_unchanged() {
        // The last leg of the same journey exec_argv starts: the bytes have to
        // survive being made into the C strings exec takes.
        let base = scratch("bytes");
        let out = base.join("arg");
        spawn_sh(
            r#"printf '%s\n' "$1" > "$0""#,
            &out,
            &[OsStr::from_bytes(b"caf\xe9.txt")],
        );
        assert_eq!(written_line(&out), b"caf\xe9.txt\n");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// A miniature XDG tree: `<base>/share/applications` holding entries,
    /// `<base>/config` holding the associations file, and `<base>/bin` standing
    /// in for `PATH`.
    fn fake_desktop(tag: &str, mimeapps: &str, entries: &[(&str, &str)]) -> (PathBuf, Dirs) {
        let base = scratch(tag);
        let apps = base.join("share").join("applications");
        std::fs::create_dir_all(&apps).expect("applications dir");
        let config = base.join("config");
        std::fs::create_dir_all(&config).expect("config dir");
        std::fs::create_dir_all(base.join("bin")).expect("bin dir");
        std::fs::write(config.join("mimeapps.list"), mimeapps).expect("mimeapps.list");
        for (name, text) in entries {
            std::fs::write(apps.join(name), text).expect("desktop entry");
        }
        let dirs = Dirs {
            data: vec![base.join("share")],
            config: Some(config),
            bin: vec![base.join("bin")],
        };
        (base, dirs)
    }

    /// Put an executable called `name` in the tree's `bin`.
    fn install_program(base: &Path, name: &str) {
        let path = base.join("bin").join(name);
        std::fs::write(&path, "#!/bin/sh\n").expect("program");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// A minimal application entry running `exec`, with `extra` lines added.
    fn app(exec: &str, extra: &str) -> String {
        format!("[Desktop Entry]\nType=Application\nExec={exec}\n{extra}")
    }

    #[test]
    fn resolution_walks_from_the_associations_to_a_real_entry() {
        // The whole first step against a tree on disk: the default names an
        // application that is not installed, so the next candidate wins.
        let (base, dirs) = fake_desktop(
            "resolve",
            "[Default Applications]\nimage/png=absent.desktop;present.desktop;\n",
            &[(
                "present.desktop",
                "[Desktop Entry]\nType=Application\nName=Present\nExec=present %f\nMimeType=image/png;\n",
            )],
        );
        let entry = resolve_handler(&dirs, "image/png").expect("a handler resolves");
        assert_eq!(entry.id, "present.desktop");
        assert_eq!(
            exec_argv(&entry.exec, Path::new("/p.png")),
            vec!["present", "/p.png"]
        );
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn an_association_that_cannot_be_launched_yields_to_the_next() {
        // Each of the first three exists on disk and is named ahead of the real
        // one, and none of them may be picked: a deleted application, one whose
        // program was uninstalled, and something that is not a program.
        let (base, dirs) = fake_desktop(
            "unlaunchable",
            "[Default Applications]\n\
             image/png=hidden.desktop;stale.desktop;link.desktop;good.desktop;\n",
            &[
                ("hidden.desktop", &app("hidden %f", "Hidden=true\n")),
                ("stale.desktop", &app("stale %f", "TryExec=stale\n")),
                (
                    "link.desktop",
                    "[Desktop Entry]\nType=Link\nURL=https://example.invalid/\nExec=link %f\n",
                ),
                ("good.desktop", &app("good %f", "TryExec=good\n")),
            ],
        );
        install_program(&base, "good");
        let entry = resolve_handler(&dirs, "image/png").expect("a handler resolves");
        assert_eq!(entry.id, "good.desktop");

        // TryExec is what decided: once its program is installed, the stale
        // entry is ahead of the good one again.
        install_program(&base, "stale");
        let entry = resolve_handler(&dirs, "image/png").expect("a handler resolves");
        assert_eq!(entry.id, "stale.desktop");

        // A program that is there but may not be executed is as good as absent.
        let stale = base.join("bin").join("stale");
        std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let entry = resolve_handler(&dirs, "image/png").expect("a handler resolves");
        assert_eq!(entry.id, "good.desktop");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_hidden_entry_masks_the_same_id_in_a_lower_directory() {
        // Hidden=true in the user's directory is how an application installed
        // system-wide is deleted for one user. Falling through to the system
        // copy of the same id would undo that.
        let (base, mut dirs) = fake_desktop(
            "mask",
            "[Default Applications]\napplication/pdf=viewer.desktop;other.desktop;\n",
            &[(
                "viewer.desktop",
                &app("user-viewer %f", "Hidden=true\nMimeType=application/pdf;\n"),
            )],
        );
        let system = base.join("system");
        let system_apps = system.join("applications");
        std::fs::create_dir_all(&system_apps).expect("system applications dir");
        std::fs::write(
            system_apps.join("viewer.desktop"),
            app("system-viewer %f", "MimeType=application/pdf;\n"),
        )
        .expect("system viewer");
        dirs.data.push(system.clone());

        // Neither the association nor the scan may bring the system copy back.
        assert!(resolve_handler(&dirs, "application/pdf").is_none());

        // The next association is in the lower directory and is not masked.
        std::fs::write(system_apps.join("other.desktop"), app("other %f", "")).expect("other");
        let entry = resolve_handler(&dirs, "application/pdf").expect("a handler resolves");
        assert_eq!(entry.id, "other.desktop");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_type_nothing_is_registered_for_falls_back_to_scanning_the_entries() {
        // No association names a handler for this type, so every entry is read
        // and the one claiming it in MimeType= is found.
        let (base, dirs) = fake_desktop(
            "scan",
            "[Default Applications]\nimage/png=other.desktop;\n",
            &[
                (
                    "other.desktop",
                    "[Desktop Entry]\nType=Application\nExec=other %f\nMimeType=image/png;\n",
                ),
                (
                    "reader.desktop",
                    "[Desktop Entry]\nType=Application\nExec=reader %f\nMimeType=application/pdf;\n",
                ),
                (
                    "hidden.desktop",
                    "[Desktop Entry]\nType=Application\nNoDisplay=true\nExec=nope %f\nMimeType=application/pdf;\n",
                ),
            ],
        );
        let entry = resolve_handler(&dirs, "application/pdf").expect("the scan finds it");
        assert_eq!(entry.id, "reader.desktop");
        // A type genuinely nobody handles resolves to nothing rather than to
        // whichever entry happened to be read first.
        assert!(resolve_handler(&dirs, "audio/flac").is_none());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_type_with_no_handler_opens_with_its_parent_types_handler() {
        let (base, dirs) = fake_desktop(
            "subclass",
            "[Default Applications]\ntext/plain=editor.desktop;\napplication/zip=archiver.desktop;\n",
            &[
                ("editor.desktop", &app("editor %f", "")),
                ("archiver.desktop", &app("archiver %f", "")),
                (
                    "rusty.desktop",
                    &app("rusty %f", "MimeType=text/x-special;\n"),
                ),
            ],
        );
        // Any text type is text/plain, with no table needed to say so.
        let entry = resolve_handler(&dirs, "text/rust").expect("the text fallback");
        assert_eq!(entry.id, "editor.desktop");
        // But a handler for the type itself still outranks the parent's.
        let entry = resolve_handler(&dirs, "text/x-special").expect("its own handler");
        assert_eq!(entry.id, "rusty.desktop");
        // Nothing says what an unknown non-text type derives from.
        assert!(resolve_handler(&dirs, "application/x-comic").is_none());

        // The subclasses table supplies the rest, parents of parents included.
        let mime_dir = base.join("share").join("mime");
        std::fs::create_dir_all(&mime_dir).expect("mime dir");
        std::fs::write(
            mime_dir.join("subclasses"),
            "application/x-comic application/x-comic-base\n\
             application/x-comic-base application/zip\n\
             application/x-loop application/x-loop\n",
        )
        .expect("subclasses");
        assert_eq!(
            ancestors(&dirs, "application/x-comic"),
            vec!["application/x-comic-base", "application/zip"]
        );
        let entry = resolve_handler(&dirs, "application/x-comic").expect("the grandparent");
        assert_eq!(entry.id, "archiver.desktop");
        // A type listed as its own parent ends the walk instead of spinning it.
        assert!(ancestors(&dirs, "application/x-loop").is_empty());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_relative_path_is_made_absolute_before_it_is_handed_over() {
        let (base, dirs) = fake_desktop("absolute", "", &[]);
        let here = std::env::current_dir().expect("a working directory");
        let absolute = here.join("notes").join("a b.pdf");
        let entry = |exec: &str| Entry {
            id: "viewer.desktop".to_string(),
            exec: exec.to_string(),
            terminal: false,
        };
        // "file://./notes/a b.pdf" is not a URL anything can open, and a bare
        // relative path means something else in the application's own
        // working directory.
        assert_eq!(
            command_for(&entry("viewer %u"), Path::new("./notes/a b.pdf"), &dirs).expect("argv"),
            vec![
                OsString::from("viewer"),
                OsString::from(file_url(&absolute))
            ]
        );
        assert_eq!(
            command_for(&entry("viewer %f"), Path::new("notes/a b.pdf"), &dirs).expect("argv"),
            vec![OsStr::new("viewer"), absolute.as_os_str()]
        );
        // An Exec with nothing in it is refused rather than run as a bare path.
        assert!(command_for(&entry(" "), Path::new("/a.pdf"), &dirs).is_err());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_terminal_application_is_started_through_the_terminal_runner() {
        let (base, dirs) = fake_desktop(
            "terminal",
            "[Default Applications]\ntext/plain=vi.desktop;\n",
            &[("vi.desktop", &app("vi %f", "Terminal=true\n"))],
        );
        let entry = resolve_handler(&dirs, "text/plain").expect("a handler resolves");
        assert!(entry.terminal);

        // With no runner installed the open is refused, and says why: exec'd
        // bare, the editor would start on /dev/null with no window.
        let err = command_for(&entry, Path::new("/notes.txt"), &dirs).expect_err("no runner");
        assert!(
            err.to_string().contains(TERMINAL_RUNNER),
            "the error does not name the runner: {err}"
        );

        install_program(&base, TERMINAL_RUNNER);
        assert_eq!(
            command_for(&entry, Path::new("/notes.txt"), &dirs).expect("argv"),
            vec![TERMINAL_RUNNER, "vi", "/notes.txt"]
        );
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn data_dirs_fall_back_to_the_spec_default() {
        // Whatever this machine's environment says, the list is absolute paths
        // and ends at the system directories.
        let dirs = data_dirs();
        assert!(dirs.iter().all(|d| d.is_absolute()));
        assert!(
            dirs.iter().any(|d| d.ends_with("share")),
            "no share directory in {dirs:?}"
        );
    }
}
