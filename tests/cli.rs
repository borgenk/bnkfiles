//! End-to-end checks on the binary itself, for the paths that reach a verdict
//! before any window is opened.
//!
//! These run the real executable, so there is no mocking and no arrangement: what
//! the test sees is what a person typing the command sees. They need no compositor
//! and no GPU, because everything they exercise happens before `app::run` connects
//! to anything.

use std::process::Command;

/// A path under the system temp directory that is guaranteed not to exist.
fn missing_path(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("bnkfiles-absent-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::remove_file(&path);
    path
}

#[test]
fn a_path_that_is_not_there_reports_once_and_exits_nonzero() {
    let missing = missing_path("startup");
    let out = Command::new(env!("CARGO_BIN_EXE_bnkfiles"))
        .arg(&missing)
        .output()
        .expect("run bnkfiles");

    assert_eq!(
        out.status.code(),
        Some(1),
        "a path that cannot be opened must exit nonzero"
    );
    assert!(
        out.stdout.is_empty(),
        "an error belongs on stderr, not stdout"
    );

    // Exactly one line: a missing file is reported before any window opens, and
    // not again by the decoder or the preloader.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "expected one error line, got: {stderr:?}");
    assert!(
        lines[0].starts_with("bnkfiles: "),
        "the error names the program: {:?}",
        lines[0]
    );
    assert!(
        lines[0].contains("bnkfiles-absent-startup"),
        "the error names the path the user typed: {:?}",
        lines[0]
    );
}

#[test]
fn the_error_names_the_path_even_when_its_parent_is_missing_too() {
    // A whole missing branch, not just a missing leaf: the message still points at
    // what was typed rather than at whichever ancestor the syscall stopped on.
    let missing = missing_path("deep").join("nested").join("photo.jpg");
    let out = Command::new(env!("CARGO_BIN_EXE_bnkfiles"))
        .arg(&missing)
        .output()
        .expect("run bnkfiles");

    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("photo.jpg"),
        "the error names the path the user typed: {stderr:?}"
    );
    assert_eq!(stderr.lines().filter(|l| !l.trim().is_empty()).count(), 1);
}
