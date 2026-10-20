//! The Terminals page: what each shared terminal at this
//! computer is showing, read from the PTY session's own screen model, and
//! starting a new shell, Claude Code or Codex session at the desk.
//!
//! [`agent_chip`] and [`last_lines`] decide what a tile says and have no GTK
//! in them, so they are unit-tested.

use crate::device_links::pty::registry::{registry, SessionInfo};
use crate::device_links::pty::session::Resync;

/// How many lines of output a preview tile shows.
pub(crate) const PREVIEW_LINES: usize = 5;

/// The registry owner of sessions started at the desk (`desk.rs`'s
/// `DESK_OWNER`), so agent sessions count against the desk's own cap.
const DESK_OWNER: &str = "desk";

/// Every live session at this computer, newest first.
pub(crate) fn sessions() -> Vec<SessionInfo> {
    let Ok(registry) = registry().lock() else {
        return Vec::new();
    };
    let mut sessions = registry.list();
    let started = |info: &SessionInfo| registry.get(&info.id).map(|session| session.started());
    sessions.sort_by_cached_key(|info| std::cmp::Reverse(started(info)));
    sessions
}

/// When session `id` started, as local "HH:MM".
pub(crate) fn started(id: &str) -> Option<String> {
    let started = registry().lock().ok()?.get(id)?.started();
    let seconds = started
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let local = gtk::glib::DateTime::from_unix_local(seconds as i64).ok()?;
    Some(local.format("%H:%M").ok()?.to_string())
}

/// Ends session `id` and everything running in it.
pub(crate) fn close(id: &str) {
    if let Ok(mut registry) = registry().lock() {
        registry.close(id);
    }
}

/// The last lines `id` is showing, and whether it printed anything lately.
pub(crate) fn preview(info: &SessionInfo) -> (Vec<String>, bool) {
    screen_lines(info, PREVIEW_LINES)
}

/// The last `count` lines `id` is showing, and whether it printed anything
/// lately.
pub(crate) fn screen_lines(info: &SessionInfo, count: usize) -> (Vec<String>, bool) {
    let Some(session) = registry()
        .lock()
        .ok()
        .and_then(|registry| registry.get(&info.id))
    else {
        return (Vec::new(), false);
    };
    let live = session.idle_for() < std::time::Duration::from_secs(3);
    // A snapshot is the current screen as escape codes; replaying it into a
    // screen model of the same size gives its text.
    let data = match session.resync(None, 0) {
        Ok(Resync::Snapshot { data, .. }) | Ok(Resync::Bytes { data, .. }) => data,
        Err(_) => return (Vec::new(), live),
    };
    let mut parser = vt100::Parser::new(info.rows.max(1), info.cols.max(1), 0);
    parser.process(&data);
    (last_lines(&parser.screen().contents(), count), live)
}

/// The last `count` lines of a screen, without the blank rows below the
/// cursor or trailing spaces.
pub(crate) fn last_lines(contents: &str, count: usize) -> Vec<String> {
    let lines: Vec<&str> = contents.lines().map(str::trim_end).collect();
    let end = lines
        .iter()
        .rposition(|line| !line.is_empty())
        .map_or(0, |last| last + 1);
    lines[..end]
        .iter()
        .skip(end.saturating_sub(count))
        .map(|line| line.to_string())
        .collect()
}

/// The small label on a tile: "CC" for Claude Code, "CX" for Codex, ">_" for
/// a shell.
pub(crate) fn agent_chip(title: &str) -> &'static str {
    let program = title
        .rsplit('/')
        .next()
        .unwrap_or(title)
        .trim_start_matches('-');
    match program {
        "claude" => "CC",
        "codex" => "CX",
        _ => ">_",
    }
}

/// "Claude Code", "Codex" or the shell's name.
pub(crate) fn display_title(title: &str) -> String {
    match agent_chip(title) {
        "CC" => "Claude Code".to_string(),
        "CX" => "Codex".to_string(),
        _ => title.trim_start_matches('-').to_string(),
    }
}

/// The agent programs this computer can run ("claude", "codex").
pub(crate) fn installed_agents() -> Vec<String> {
    crate::device_links::pty::launchers::installed()
}

/// Starts a session at the desk — `None` for the login shell, else the named
/// agent — and opens its window; phones list it too.
pub(crate) fn start(program: Option<&str>) -> Result<String, String> {
    let Some(program) = program else {
        return crate::device_links::pty::desk::new_desk_session();
    };
    let info = registry()
        .lock()
        .map_err(|_| "terminal registry lock poisoned".to_string())?
        .create(
            Some(portable_pty::CommandBuilder::new(program)),
            24,
            80,
            DESK_OWNER,
        )?;
    crate::device_links::pty::desk::open_on_desktop(&info.id);
    Ok(info.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tile_shows_the_last_lines_above_the_cursor() {
        let screen = "one\ntwo\nthree\nfour\nfive\nsix   \n\n\n";
        assert_eq!(
            last_lines(screen, 3),
            vec!["four".to_string(), "five".to_string(), "six".to_string()]
        );
        assert_eq!(last_lines("", 5), Vec::<String>::new());
        assert_eq!(last_lines("only\n", 5), vec!["only".to_string()]);
    }

    #[test]
    fn agents_are_told_apart_from_shells() {
        assert_eq!(agent_chip("claude"), "CC");
        assert_eq!(agent_chip("/usr/bin/codex"), "CX");
        assert_eq!(agent_chip("-zsh"), ">_");
        assert_eq!(display_title("claude"), "Claude Code");
        assert_eq!(display_title("-bash"), "bash");
    }
}
