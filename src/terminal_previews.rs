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

/// How many of the last lines say whether a terminal is waiting for the
/// person (the phone's `TerminalAttention` looks at the last 14 too).
pub(crate) const ATTENTION_LINES: usize = 14;

/// Whether the last lines of a terminal ask the person something: a y/n
/// question, an approval menu, "press enter", a password prompt. The same
/// prompts as the phone's `classifyAttention` (`TerminalAttention.kt`); only
/// the last [`ATTENTION_LINES`] non-blank lines count, a prompt that scrolled
/// away is history.
pub(crate) fn waiting_for_input(lines: &[String]) -> bool {
    let tail: Vec<&String> = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .collect();
    tail[tail.len().saturating_sub(ATTENTION_LINES)..]
        .iter()
        .any(|line| asks_the_person(line))
}

/// Starts with one of `words` as a whole word; returns what follows.
fn after_word<'a>(text: &'a str, words: &[&str]) -> Option<&'a str> {
    words.iter().find_map(|word| {
        let rest = text.strip_prefix(word)?;
        (rest.is_empty() || !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
            .then_some(rest)
    })
}

/// The text after each place `phrase` occurs as whole words (`\bphrase\b`).
fn after_phrase<'a>(haystack: &'a str, phrase: &'a str) -> impl Iterator<Item = &'a str> {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    haystack.match_indices(phrase).filter_map(move |(at, _)| {
        let rest = &haystack[at + phrase.len()..];
        let starts = !haystack[..at].chars().next_back().is_some_and(word);
        let ends = !rest.chars().next().is_some_and(word);
        (starts && ends).then_some(rest)
    })
}

/// The text of every place `needle` occurs in `haystack`, from just after it.
fn after_each<'a>(haystack: &'a str, needle: &'a str) -> impl Iterator<Item = &'a str> {
    haystack
        .match_indices(needle)
        .map(move |(at, _)| &haystack[at + needle.len()..])
}

fn asks_the_person(line: &str) -> bool {
    let lower = line.to_lowercase();
    let squeezed: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    // (y/n) [Y/n] [y/N] (yes/no)
    if [
        "(y/n)", "[y/n]", "(yes/no)", "[yes/no]", "(y/no)", "[y/no]", "(yes/n)", "[yes/n]",
    ]
    .iter()
    .any(|mark| squeezed.contains(mark))
    {
        return true;
    }
    // Claude Code, Codex.
    let starts = |phrase: &str, words: &[&str]| {
        after_each(&lower, phrase).any(|rest| after_word(rest, words).is_some())
    };
    if starts(
        "do you want to ",
        &[
            "proceed",
            "continue",
            "make this edit",
            "create",
            "run",
            "allow",
            "overwrite",
            "apply",
            "delete",
        ],
    ) || starts(
        "would you like to ",
        &["proceed", "continue", "run", "apply", "allow", "make"],
    ) {
        return true;
    }
    // allow (this )?(command|codex|claude|tool) ... ?
    for rest in after_each(&lower, "allow ") {
        let rest = rest.strip_prefix("this ").unwrap_or(rest);
        if after_word(rest, &["command", "codex", "claude", "tool"])
            .is_some_and(|after| after.contains('?'))
        {
            return true;
        }
    }
    if ["press enter", "press return", "press any key", "hit enter"]
        .iter()
        .any(|phrase| after_phrase(&lower, phrase).next().is_some())
    {
        return true;
    }
    let trimmed = lower.trim_end();
    if trimmed.ends_with("continue?") || trimmed.ends_with("proceed?") {
        return true;
    }
    if after_phrase(&lower, "are you sure").any(|rest| rest.contains('?')) {
        return true;
    }
    // An approval menu with its cursor on "1. Yes".
    let menu = lower
        .trim_start()
        .trim_start_matches(['❯', '>', '›'])
        .trim_start();
    if menu.strip_prefix("1.").is_some_and(|rest| {
        after_word(rest.trim_start(), &["yes"]).is_some() && rest.starts_with(char::is_whitespace)
    }) {
        return true;
    }
    // "3. No, and tell Claude what to do differently".
    let word_before = |at: usize| {
        lower[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    };
    if lower.match_indices("no, and tell ").any(|(at, phrase)| {
        let rest = &lower[at + phrase.len()..];
        let word_end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        !word_before(at) && word_end > 0 && rest[word_end..].starts_with(" what to do")
    }) {
        return true;
    }
    // [sudo] password for sam:   Passphrase:
    if let Some(before) = trimmed.strip_suffix(':') {
        for word in ["password", "passphrase"] {
            for (at, _) in before.match_indices(word) {
                let starts_a_word = before[..at]
                    .chars()
                    .next_back()
                    .is_none_or(char::is_whitespace);
                let rest = &before[at + word.len()..];
                if starts_a_word
                    && (rest.is_empty() || (rest.starts_with(" for ") && !rest.contains(':')))
                {
                    return true;
                }
            }
        }
    }
    false
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

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    /// The vectors of the phone's `TerminalAttentionTest`: the same prompts
    /// wait, the same ordinary output does not.
    #[test]
    fn a_terminal_asking_something_is_waiting_for_input() {
        for prompt in [
            "Overwrite file? (y/n)",
            "Proceed [Y/n]",
            "Continue ( yes / no )",
            "Do you want to proceed?",
            "Do you want to make this edit to src/main.rs?",
            "Would you like to run the following command?",
            "Allow this command to run? ",
            "Press ENTER to continue",
            "Are you sure you want to delete it?",
            "❯ 1. Yes",
            "  3. No, and tell Claude what to do differently",
            "[sudo] password for sam:",
            "Password:",
            "Enter passphrase:",
            "Apply these changes, continue?",
        ] {
            assert!(waiting_for_input(&lines(prompt)), "{prompt}");
        }
        for output in [
            "",
            "   ",
            "$ cargo build",
            "Compiling desklink v1.4.0",
            "the password file was not changed",
            "compress enterprise logs",
            "we compress entering data and hit entertainment",
            "unpress enter",
            "they are you surely done",
            "Password rules: 12 characters",
            "You can continue later.",
            "yes/no questions are handled elsewhere",
        ] {
            assert!(!waiting_for_input(&lines(output)), "{output}");
        }
    }

    /// Only the last 14 lines count: an answered question scrolled away.
    #[test]
    fn a_prompt_that_scrolled_away_is_history() {
        let mut screen = vec!["Overwrite file? (y/n)".to_string()];
        screen.extend((0..14).map(|n| format!("line {n}")));
        assert!(!waiting_for_input(&screen));
        screen.remove(1);
        assert!(waiting_for_input(&screen));
        // Blank lines do not push it out.
        let mut blanks = vec!["Overwrite file? (y/n)".to_string()];
        blanks.extend(std::iter::repeat_n(String::new(), 30));
        assert!(waiting_for_input(&blanks));
    }

    /// The phone looks at the last 14 non-blank lines of the whole screen:
    /// blank lines between do not shorten the window.
    #[test]
    fn blank_lines_between_do_not_hide_a_prompt() {
        let mut screen = vec!["Overwrite file? (y/n)".to_string()];
        for n in 0..12 {
            screen.push(format!("line {n}"));
            screen.push(String::new());
        }
        // 13 non-blank lines in 25: the prompt is within the last 14.
        assert!(waiting_for_input(&screen));
        screen.push("one more".into());
        assert!(waiting_for_input(&screen), "the 14th from the end");
        screen.push("and another".into());
        assert!(!waiting_for_input(&screen));
    }
}
