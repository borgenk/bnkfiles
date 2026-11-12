//! The sidebar's places: home, the root, the mounted filesystems, pinned folders
//! and the collections bnkfiles has seen.
//!
//! The only interesting part is deciding what counts as a mount worth showing. A
//! desktop Linux box has forty-odd entries in `/proc/self/mounts` and three of them
//! are places a person would browse; the rest is the kernel's own plumbing (proc,
//! sysfs, cgroups, the per-process namespace mounts) plus whatever the container
//! and snap tooling has layered on. Parsing is separated from reading so the filter
//! — the part with judgement in it — is pinned by tests against a fixture rather
//! than against whatever this machine happens to have mounted.
//!
//! The filter is an allowlist of filesystem types, not a denylist of the ones seen
//! so far: a kernel that grows a new pseudo-filesystem next year should not appear
//! in the sidebar because nobody updated a list of things to hide.

use std::path::{Path, PathBuf};

/// Filesystem types a person might actually keep files on: the Linux natives, the
/// ones removable media arrive formatted as, the optical formats, and FUSE (which
/// is how MTP phones, network shares, and archive mounts present themselves).
const BROWSABLE: &[&str] = &[
    "ext2", "ext3", "ext4", "btrfs", "xfs", "zfs", "f2fs", "jfs", "reiserfs", "bcachefs", "vfat",
    "exfat", "msdos", "ntfs", "ntfs3", "iso9660", "udf", "hfsplus", "apfs", "fuseblk", "nfs",
    "nfs4", "cifs", "smb3", "sshfs",
];

/// Mount points that are infrastructure whatever they are formatted as: a container
/// runtime's layers, snap's loopbacks, the boot firmware partition. A person who
/// wants these can type the path.
const HIDDEN_PREFIXES: &[&str] = &[
    "/proc",
    "/sys",
    "/dev",
    "/run",
    "/snap",
    "/boot",
    "/var/lib/docker",
    "/var/lib/containers",
    "/var/snap",
    "/tmp",
];

/// What kind of place a sidebar row is, which decides its icon and its order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlaceKind {
    Home,
    Root,
    Mount,
    Pinned,
    /// A folder with a `.bnkfiles/` directory in it, the marker that makes it a
    /// collection.
    Collection,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Place {
    pub name: String,
    pub path: PathBuf,
    pub kind: PlaceKind,
}

/// The sidebar's rows: home, the filesystem root, the mounted volumes, then the
/// folders the user pinned. A pinned folder that is also a mount appears once, as
/// the pin, since that is the name the user chose for it.
///
/// The collections come last, in their own section. A collection is a kind of
/// folder rather than another way to reach one, so it is listed there even when
/// it is also pinned or mounted.
pub fn places(
    home: Option<&Path>,
    mounts_text: &str,
    pinned: &[PathBuf],
    collections: &[PathBuf],
) -> Vec<Place> {
    let mut out = Vec::new();
    if let Some(home) = home {
        out.push(Place {
            name: "Home".to_string(),
            path: home.to_path_buf(),
            kind: PlaceKind::Home,
        });
    }
    out.push(Place {
        name: "Filesystem".to_string(),
        path: PathBuf::from("/"),
        kind: PlaceKind::Root,
    });
    out.extend(parse_mounts(mounts_text));
    for path in pinned {
        out.push(Place {
            name: display_name(path),
            path: path.clone(),
            kind: PlaceKind::Pinned,
        });
    }
    // A path is one row however many ways it could have got here. Later entries win
    // (a pin overrides the mount it names), which is why this runs backwards.
    let mut seen = std::collections::HashSet::new();
    let mut deduped: Vec<Place> = out
        .into_iter()
        .rev()
        .filter(|p| seen.insert(p.path.clone()))
        .collect();
    deduped.reverse();
    let mut seen = std::collections::HashSet::new();
    for path in collections {
        if seen.insert(path) {
            deduped.push(Place {
                name: display_name(path),
                path: path.clone(),
                kind: PlaceKind::Collection,
            });
        }
    }
    deduped
}

/// Parse `/proc/self/mounts` into the volumes worth listing.
///
/// The format is one mount per line, space-separated: device, mount point, type,
/// options, dump, pass. The kernel escapes spaces, tabs, newlines, and backslashes
/// in the device and mount-point fields as octal, which is why a volume called
/// "My Passport" needs [`unescape`] rather than a plain split.
pub fn parse_mounts(text: &str) -> Vec<Place> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(_device), Some(point), Some(fstype)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !BROWSABLE.contains(&fstype) && !fstype.starts_with("fuse.") {
            continue;
        }
        let point = unescape(point);
        // The root is already its own row, and a mount inside the kernel's or a
        // container runtime's territory is not somewhere to browse.
        if point == "/" || HIDDEN_PREFIXES.iter().any(|p| is_under(&point, p)) {
            continue;
        }
        let path = PathBuf::from(&point);
        out.push(Place {
            name: display_name(&path),
            path,
            kind: PlaceKind::Mount,
        });
    }
    out
}

/// Whether `path` is `prefix` or sits inside it. Compared by path component, so
/// `/runners` is not treated as living under `/run`.
fn is_under(path: &str, prefix: &str) -> bool {
    match path.strip_prefix(prefix) {
        Some("") => true,
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

/// Undo the kernel's octal escaping of the characters that would otherwise split a
/// field. A backslash that does not begin a valid three-digit escape is a literal
/// backslash, which is what a filesystem is allowed to contain.
fn unescape(field: &str) -> String {
    if !field.contains('\\') {
        return field.to_string();
    }
    let bytes = field.as_bytes();
    let mut out = String::with_capacity(field.len());
    let mut i = 0;
    while i < bytes.len() {
        let escape = (bytes[i] == b'\\' && i + 3 < bytes.len())
            .then(|| std::str::from_utf8(&bytes[i + 1..i + 4]).ok())
            .flatten()
            .filter(|digits| digits.bytes().all(|b| (b'0'..=b'7').contains(&b)))
            .and_then(|digits| u8::from_str_radix(digits, 8).ok());
        match escape {
            Some(byte) => {
                out.push(byte as char);
                i += 4;
            }
            None => {
                // Not an escape: copy the character whole, so a multi-byte one is
                // not split down the middle.
                let ch = field[i..].chars().next().unwrap_or('\\');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// The name to show for a path: its last component, or the path itself for the
/// root and anything else with no last component to speak of.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Read the mount table. Separated from [`parse_mounts`] so the parsing, which is
/// where the judgement lives, is testable without this machine's mounts.
pub fn read_mounts() -> String {
    std::fs::read_to_string("/proc/self/mounts").unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use crate::places::*;

    /// A mount table with the shape of a real desktop one: the kernel's plumbing,
    /// a container runtime, snap loopbacks, and three volumes worth showing.
    const FIXTURE: &str = "\
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
sys /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0
udev /dev devtmpfs rw,nosuid,relatime,size=16384k 0 0
/dev/nvme0n1p2 / ext4 rw,relatime 0 0
tmpfs /run tmpfs rw,nosuid,nodev,size=3276800k 0 0
cgroup2 /sys/fs/cgroup cgroup2 rw,nosuid,nodev,noexec,relatime 0 0
/dev/nvme0n1p1 /boot/efi vfat rw,relatime,fmask=0022 0 0
/dev/sda1 /media/My\\040Passport exfat rw,relatime,uid=1000 0 0
/dev/sdb2 /mnt/archive btrfs rw,relatime,ssd,space_cache=v2 0 0
/dev/loop3 /snap/firefox/4321 squashfs ro,nodev,relatime 0 0
overlay /var/lib/docker/overlay2/abc/merged overlay rw,relatime 0 0
tmpfs /run/user/1000 tmpfs rw,nosuid,nodev,relatime,size=3276800k 0 0
nas:/export/photos /mnt/photos nfs4 rw,relatime,vers=4.2 0 0
portal /run/user/1000/doc fuse.portal rw,nosuid,nodev,relatime 0 0
sshfs#me@box:/ /home/me/remote fuse.sshfs rw,nosuid,nodev,relatime 0 0
";

    fn names(places: &[Place]) -> Vec<&str> {
        places.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn only_the_volumes_a_person_would_browse_survive_the_filter() {
        let mounts = parse_mounts(FIXTURE);
        assert_eq!(
            names(&mounts),
            vec!["My Passport", "archive", "photos", "remote"]
        );
        assert!(mounts.iter().all(|m| m.kind == PlaceKind::Mount));
    }

    #[test]
    fn the_kernels_own_plumbing_never_appears() {
        let mounts = parse_mounts(FIXTURE);
        let paths: Vec<String> = mounts
            .iter()
            .map(|m| m.path.to_string_lossy().into_owned())
            .collect();
        for hidden in [
            "/proc",
            "/sys",
            "/dev",
            "/run",
            "/sys/fs/cgroup",
            "/run/user/1000",
            // A FUSE mount is browsable by type, but not when it is the portal's
            // plumbing under /run.
            "/run/user/1000/doc",
            // vfat and squashfs and overlay, excluded by where they are or what
            // they are rather than by being unbrowsable in principle.
            "/boot/efi",
            "/snap/firefox/4321",
            "/var/lib/docker/overlay2/abc/merged",
            // The root has its own row and must not be duplicated as a mount.
            "/",
        ] {
            assert!(
                !paths.contains(&hidden.to_string()),
                "{hidden} should not be a sidebar place, got {paths:?}"
            );
        }
    }

    #[test]
    fn an_escaped_mount_point_comes_back_with_its_real_name() {
        // The kernel writes a space as \040; a volume called "My Passport" would
        // otherwise parse as a mount point of "My" and a filesystem type of "0040...".
        let mounts = parse_mounts("/dev/sda1 /media/My\\040Passport exfat rw 0 0\n");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].path, PathBuf::from("/media/My Passport"));
        assert_eq!(mounts[0].name, "My Passport");
    }

    #[test]
    fn unescaping_handles_the_other_escapes_and_leaves_junk_alone() {
        assert_eq!(unescape("/plain/path"), "/plain/path");
        assert_eq!(unescape("a\\040b"), "a b");
        assert_eq!(unescape("a\\011b"), "a\tb");
        assert_eq!(unescape("a\\012b"), "a\nb");
        assert_eq!(unescape("a\\134b"), "a\\b");
        // A trailing or malformed escape is a literal backslash, not a panic and
        // not a swallowed character.
        assert_eq!(unescape("a\\"), "a\\");
        assert_eq!(unescape("a\\09"), "a\\09");
        assert_eq!(unescape("a\\999b"), "a\\999b");
    }

    #[test]
    fn a_short_or_ragged_line_is_skipped_rather_than_guessed_at() {
        // /proc is not a file format with a version, so anything unexpected is
        // dropped rather than half-parsed.
        assert!(parse_mounts("").is_empty());
        assert!(parse_mounts("garbage\n").is_empty());
        assert!(parse_mounts("dev /mnt/x\n").is_empty(), "no type field");
        assert!(parse_mounts("\n\n   \n").is_empty());
        // A good line among bad ones still comes through.
        let mixed = parse_mounts("junk\n/dev/sdb2 /mnt/archive btrfs rw 0 0\nmore junk\n");
        assert_eq!(names(&mixed), vec!["archive"]);
    }

    #[test]
    fn is_under_compares_whole_components() {
        assert!(is_under("/run", "/run"));
        assert!(is_under("/run/user/1000", "/run"));
        // The bug this exists to prevent: a prefix match on the raw string would
        // hide /runners and /devices along with /run and /dev.
        assert!(!is_under("/runners", "/run"));
        assert!(!is_under("/devices/x", "/dev"));
        assert!(!is_under("/mnt/archive", "/run"));
    }

    #[test]
    fn the_sidebar_leads_with_home_and_the_root_then_pins() {
        let home = PathBuf::from("/home/me");
        let pinned = vec![PathBuf::from("/home/me/Pictures")];
        let all = places(Some(&home), FIXTURE, &pinned, &[]);
        assert_eq!(
            names(&all),
            vec![
                "Home",
                "Filesystem",
                "My Passport",
                "archive",
                "photos",
                "remote",
                "Pictures"
            ]
        );
        assert_eq!(all[0].kind, PlaceKind::Home);
        assert_eq!(all[1].kind, PlaceKind::Root);
        assert_eq!(all[6].kind, PlaceKind::Pinned);
    }

    #[test]
    fn a_place_reachable_two_ways_is_listed_once_under_the_name_the_user_chose() {
        // Pinning a mounted volume should rename that row, not add a second one
        // pointing at the same directory.
        let pinned = vec![PathBuf::from("/mnt/archive")];
        let all = places(None, FIXTURE, &pinned, &[]);
        let archive: Vec<&Place> = all
            .iter()
            .filter(|p| p.path == Path::new("/mnt/archive"))
            .collect();
        assert_eq!(archive.len(), 1, "listed once, got {all:?}");
        assert_eq!(archive[0].kind, PlaceKind::Pinned);
        // And the order of the surviving rows is otherwise untouched.
        assert_eq!(
            names(&all),
            vec!["Filesystem", "My Passport", "photos", "remote", "archive"]
        );
    }

    #[test]
    fn a_machine_with_no_home_still_gets_a_sidebar() {
        let all = places(None, "", &[], &[]);
        assert_eq!(names(&all), vec!["Filesystem"]);
    }

    #[test]
    fn a_collection_is_listed_once_even_where_it_is_also_pinned() {
        let refs = PathBuf::from("/mnt/archive/refs");
        let pinned = vec![refs.clone()];
        let all = places(None, "", &pinned, &[refs.clone(), refs.clone()]);
        let kinds: Vec<PlaceKind> = all
            .iter()
            .filter(|p| p.path == refs)
            .map(|p| p.kind)
            .collect();
        assert_eq!(kinds, [PlaceKind::Pinned, PlaceKind::Collection]);
        assert_eq!(all.last().map(|p| p.name.as_str()), Some("refs"));
    }
}
