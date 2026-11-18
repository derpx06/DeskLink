//! Every string of the update popup and its Settings rows (sentence case,
//! "computer" and "phone", no jargon). One place, so a test reads them all.
//!
//! Sibling edges: none.

pub(super) const HOW_TO_UPDATE: &str = "How to update";
pub(super) const REMIND_TOMORROW: &str = "Remind me tomorrow";
pub(super) const SKIP_VERSION: &str = "Skip this version";
pub(super) const DONE: &str = "Done";
pub(super) const COPY_COMMAND: &str = "Copy command";
pub(super) const COMMAND_COPIED: &str = "Command copied";
pub(super) const TO_UPDATE_RUN: &str = "To update, run:";
pub(super) const NEVER_BY_ITSELF: &str = "DeskLink never downloads or installs updates by itself.";
pub(super) const RELEASE_NOTES: &str = "Release notes";
pub(super) const OPEN_DOWNLOAD_PAGE: &str = "Open the download page";
pub(super) const REQUIRED_TITLE: &str = "Update needed";
pub(super) const REQUIRED_BODY: &str =
    "This version of DeskLink can't connect to your phone any more. Update to keep using it.";

pub(super) const CHECK_ROW: &str = "Check for updates";
pub(super) const CHECK_BUTTON: &str = "Check";
pub(super) const CHECKING: &str = "Checking for updates…";
pub(super) const UP_TO_DATE: &str = "You're up to date";
pub(super) const CHECK_FAILED: &str = "Couldn't check for updates. Try again later.";
pub(super) const AUTO_ROW: &str = "Check automatically";
pub(super) const AUTO_SUB: &str = "Once a day. Sends no personal information.";
pub(super) const AUTO_OFF_SUB: &str = "Automatic checks are off · You can still check by hand.";
pub(super) const SKIPPED_SUB: &str = "DeskLink will tell you about newer versions.";
pub(super) const CHANNEL_ROW: &str = "Update channel";
pub(super) const CHANNEL_BETA: &str = "Beta";
pub(super) const CHANNEL_STABLE: &str = "Stable";
pub(super) const STABLE_LATER: &str = "Stable starts at 1.0. Until then DeskLink is in beta.";
pub(super) const UNDO: &str = "Undo";
pub(super) const NOT_SET_UP: &str = "Updates aren't set up in this build";
pub(super) const SHARE_NOTE: &str = "DeskLink asks its update server for the newest version once a day. The request carries no identifier: it does not include your name, your device ID or anything about your phones. A random number kept only on this computer decides when a staged release reaches you, and is never sent. The server can see your network address, as any website can. The answer is signed, and DeskLink ignores anything that is not.";
pub(super) const GROUP_TITLE: &str = "Updates";
pub(super) const WHAT_A_CHECK_SENDS: &str = "What a check sends";
pub(super) const CHANNEL_SUB: &str = "Which releases DeskLink tells you about.";

pub(super) fn available_title(version: &str) -> String {
    format!("DeskLink {version} is available")
}

pub(super) fn installed_with(source: &str) -> String {
    format!("Installed with {source}")
}

pub(super) fn may_not_have_yet(version: &str) -> String {
    format!("Your distribution may not have {version} yet.")
}

pub(super) fn skipped_row(version: &str) -> String {
    format!("Skipped {version}")
}

pub(super) fn up_to_date_detail(version: &str, checked: &str) -> String {
    format!("DeskLink {version} · Checked {checked}")
}

/// "from 0.3.0 beta to 0.4.0 beta": what a screen reader says for the chips.
pub(super) fn chips_spoken(from: &str, to: &str) -> String {
    format!("From {from} to {to}")
}

pub(super) fn skew_older(version: &str) -> String {
    format!("Runs DeskLink {version}. Update it for the newest features.")
}

pub(super) fn skew_newer(version: &str) -> String {
    format!("Runs a newer DeskLink ({version}). Update this computer.")
}
