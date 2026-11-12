//! The notification line: one message at a time about something that has
//! just happened, said in the status bar for a while and then gone.
//!
//! Everything the app has to tell a person in passing comes through here, at
//! one of three levels: a confirmation (`Info`), something that happened
//! without being asked for and changed what is on screen (`Warning`), and
//! something asked for that failed (`Error`). The level picks the line's
//! colour and how long it stays: the more it matters, the longer it gives a
//! person to read it. A newer notice replaces the one up, since a queue of
//! stale ones would only delay the news that matters now.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warning,
    Error,
}

impl Level {
    /// How long a notice at this level stays up.
    fn linger(self) -> Duration {
        Duration::from_secs(match self {
            Level::Info => 5,
            Level::Warning => 8,
            Level::Error => 12,
        })
    }

    /// The level's name on the probe line.
    pub fn key(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warning => "warning",
            Level::Error => "error",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub level: Level,
    pub text: String,
    until: Instant,
}

impl Notice {
    /// A notice put up at `now`.
    pub fn new(level: Level, text: String, now: Instant) -> Self {
        Self {
            level,
            text,
            until: now + level.linger(),
        }
    }

    /// When it comes down: one of the event loop's deadlines.
    pub fn until(&self) -> Instant {
        self.until
    }

    /// Whether it is still up at `now`.
    pub fn is_up(&self, now: Instant) -> bool {
        now < self.until
    }
}

#[cfg(test)]
mod tests {
    use crate::notice::*;

    #[test]
    fn a_notice_that_matters_more_stays_up_longer() {
        let now = Instant::now();
        let up_for = |level| Notice::new(level, String::new(), now).until() - now;
        assert!(up_for(Level::Info) < up_for(Level::Warning));
        assert!(up_for(Level::Warning) < up_for(Level::Error));

        let info = Notice::new(Level::Info, "opening a.pdf".to_string(), now);
        assert!(info.is_up(now));
        assert!(info.is_up(info.until() - Duration::from_millis(1)));
        assert!(!info.is_up(info.until()), "down at its deadline, not after");
    }
}
