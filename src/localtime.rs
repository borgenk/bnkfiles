//! Unix timestamps in the user's own zone: the calendar arithmetic, and the one
//! call that asks the C library where the user is.
//!
//! Two consumers share it: the explorer's modified column (`YYYY-MM-DD HH:MM`) and
//! a `.trashinfo` file (`YYYY-MM-DDThh:mm:ss`). Each owns its format; this owns the
//! split into fields and the zone lookup underneath.
//!
//! The arithmetic is kept pure and separate from [`local_offset`], so the calendar
//! is pinned by tests against known instants while only the zone lookup, which is
//! the C library's to answer, crosses into FFI.

use std::ffi::{c_char, c_int, c_long};

/// Split a unix timestamp into year, month, day, hour, minute, second.
///
/// Howard Hinnant's `civil_from_days`, which shifts the epoch to March 1st of year
/// 0 so that the leap day falls at the *end* of a four-century cycle and the whole
/// thing becomes integer division with no table and no branching on leap years.
/// Euclidean division throughout, so a timestamp before 1970 gives the date it
/// should rather than one rounded toward zero.
pub fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, min, sec) = (
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    );

    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097); // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146_096) / 365; // [0, 399]
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153; // [0, 11], March-based
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day, hour, min, sec)
}

/// glibc's `struct tm`. Only `gmtoff` is read, but the layout has to match in full
/// or the call writes past what we gave it.
#[repr(C)]
struct Tm {
    sec: c_int,
    min: c_int,
    hour: c_int,
    mday: c_int,
    mon: c_int,
    year: c_int,
    wday: c_int,
    yday: c_int,
    isdst: c_int,
    gmtoff: c_long,
    zone: *const c_char,
}

// Declared here, beside its one caller.
extern "C" {
    fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
}

/// The local zone's offset from UTC at `secs`, in seconds.
///
/// Asked per timestamp rather than once, because the answer changes across a
/// daylight-saving boundary: a file modified in January and one modified in July
/// are an hour apart in their offsets, and showing both against the same one puts
/// a "modified" column an hour wrong for half the year.
///
/// `time_t` is 64-bit on the platforms this targets (Linux, 64-bit); a 32-bit
/// build would need this widened.
pub fn local_offset(secs: i64) -> i64 {
    let mut tm = Tm {
        sec: 0,
        min: 0,
        hour: 0,
        mday: 0,
        mon: 0,
        year: 0,
        wday: 0,
        yday: 0,
        isdst: 0,
        gmtoff: 0,
        zone: std::ptr::null(),
    };
    // SAFETY: both pointers address live locals for the duration of the call, and
    // `Tm` mirrors the layout the C library writes into. A null return means the
    // conversion failed (a timestamp outside the representable range), which UTC
    // stands in for rather than being an error worth surfacing over a file listing.
    let ok = unsafe { localtime_r(&secs as *const i64, &mut tm) };
    if ok.is_null() {
        0
    } else {
        tm.gmtoff as i64
    }
}

#[cfg(test)]
mod tests {
    use crate::localtime::*;

    #[test]
    fn the_calendar_holds_at_the_awkward_instants() {
        // Checked against known instants: the unix epoch, a leap day, a date past
        // 2038 (so a 32-bit truncation would show), and one before the epoch.
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_from_unix(1_000_000_000), (2001, 9, 9, 1, 46, 40));
        assert_eq!(civil_from_unix(1_709_164_800), (2024, 2, 29, 0, 0, 0));
        assert_eq!(civil_from_unix(2_147_483_648), (2038, 1, 19, 3, 14, 8));
        assert_eq!(civil_from_unix(4_102_444_800), (2100, 1, 1, 0, 0, 0));
        // 2100 is not a leap year and 2000 was: the century rule and the
        // four-century exception to it, which is what the era arithmetic is for.
        assert_eq!(civil_from_unix(951_782_400), (2000, 2, 29, 0, 0, 0));
        // Before 1970: euclidean division, so this is the last second of 1969 and
        // not something rounded toward zero.
        assert_eq!(civil_from_unix(-1), (1969, 12, 31, 23, 59, 59));
        assert_eq!(civil_from_unix(-86_400), (1969, 12, 31, 0, 0, 0));
    }

    #[test]
    fn the_local_offset_is_a_whole_number_of_minutes() {
        // Whatever zone this machine is in, the offset has to be sane: every zone
        // in the database is a whole number of minutes and within a day of UTC.
        let offset = local_offset(1_700_000_000);
        assert!(offset.abs() <= 86_400, "offset {offset} is not a real zone");
        assert_eq!(offset % 60, 0, "offset {offset} is not whole minutes");
    }
}
