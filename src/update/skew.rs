//! The muted "Your phone runs an older DeskLink" line under a phone's name
//! (`docs/UPDATES.md` 6.8). Informational only: it never gates a connection,
//! and the existing hard errors keep their own text.
//!
//! Sibling edges: `model` (`Version`).

use super::model::Version;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skew {
    /// The phone's `major.minor` is lower than ours.
    PhoneOlder(Version),
    /// The phone's `major.minor` is higher than ours.
    PhoneNewer(Version),
}

/// `phone` is the version text the phone said it runs (shown, never
/// trusted): absent, unparsable or a patch-only difference say nothing.
pub(crate) fn skew(ours: Version, phone: Option<&str>) -> Option<Skew> {
    let theirs = Version::parse(phone?.trim())?;
    match (theirs.0, theirs.1).cmp(&(ours.0, ours.1)) {
        std::cmp::Ordering::Less => Some(Skew::PhoneOlder(theirs)),
        std::cmp::Ordering::Greater => Some(Skew::PhoneNewer(theirs)),
        std::cmp::Ordering::Equal => None,
    }
}
