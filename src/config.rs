//! App configuration: the concrete font choices.
//!
//! Every string on screen is chrome (the status bar, a filename under a thumbnail,
//! the folder rows, a prompt), so there is one text family and it is a proportional
//! sans. Families are named, not located: fontconfig resolves each name to a file on
//! whatever machine this runs on.
//!
//! Filenames are the least predictable text an app can be handed (any script, any
//! emoji, in any folder), and none of it is pinned here: a character the chosen family
//! lacks is resolved through fontconfig (`platform/fontconfig.rs`), the way every
//! other application on the desktop resolves it.

use crate::platform::freetype::FontConfig;

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            // First installed wins. Noto Sans and Roboto lead because both ship a
            // medium cut for the labels that want one; the generic alias at the end
            // resolves on any machine with a font at all.
            families: ["Noto Sans", "Roboto", "sans-serif"]
                .map(String::from)
                .to_vec(),
            emoji: "emoji".to_string(),
        }
    }
}
