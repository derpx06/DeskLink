//! Telling a phone when a long command finishes.
//!
//! Once a second each session's foreground process group is read from the
//! PTY. A job is anything in the foreground that is not the shell itself;
//! when the shell's own group is back in front after a job ran longer than
//! [`ALERT_AFTER`], the job finished, and every phone with the terminal
//! channel open hears about it (`finished`). The phone stays quiet while
//! that terminal is on its screen.
//!
//! Not every return to the prompt is a finished command: an interactive
//! program the user left (an editor, a pager, `ssh`, a nested shell) is not
//! news, and Ctrl-Z only stopped the job.
//!
//! [`JobWatch`] decides from a sequence of observations and is unit-tested
//! with made-up ones.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Commands shorter than this finish without an alert.
pub(crate) const ALERT_AFTER: Duration = Duration::from_secs(10);

/// Programs a person leaves rather than waits for: never announced.
const INTERACTIVE: [&str; 24] = [
    "vim", "nvim", "vi", "nano", "emacs", "micro", "hx", "less", "more", "most", "man", "top",
    "htop", "btop", "ssh", "mosh", "tmux", "screen", "bash", "zsh", "fish", "sh", "dash", "watch",
];

fn interactive(name: &str) -> bool {
    INTERACTIVE.contains(&name)
}

/// One session's foreground, as seen once a second.
#[derive(Debug, Default)]
pub(crate) struct JobWatch {
    /// The job in front: its process group, when it was first seen, its name.
    job: Option<(i32, Instant, String)>,
}

impl JobWatch {
    /// Records that `foreground` is in front of the shell `shell` at `now`.
    /// Returns the job that just finished, if it ran long enough to tell, was
    /// not an interactive program, and was not merely stopped (`stopped`).
    pub(crate) fn observe(
        &mut self,
        foreground: Option<i32>,
        shell: i32,
        now: Instant,
        name: impl FnOnce(i32) -> String,
        stopped: impl FnOnce(i32) -> bool,
    ) -> Option<(String, Duration)> {
        match foreground {
            // Unknown this time (the PTY is closing): change nothing.
            None => None,
            Some(group) if group != shell => {
                if self.job.as_ref().map(|(job, _, _)| *job) != Some(group) {
                    self.job = Some((group, now, name(group)));
                }
                None
            }
            Some(_) => {
                let (group, since, job) = self.job.take()?;
                let ran = now.duration_since(since);
                (ran > ALERT_AFTER && !interactive(&job) && !stopped(group)).then_some((job, ran))
            }
        }
    }
}

/// Whether process `pid` is stopped (`T`, e.g. by Ctrl-Z).
fn is_stopped(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let state = stat.rsplit_once(')')?.1.trim_start().chars().next()?;
            Some(state == 'T' || state == 't')
        })
        .unwrap_or(false)
}

/// Starts the once-a-second watch, once.
pub(crate) fn start_watch() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("desklink-job-watch".into())
            .spawn(|| {
                let mut jobs: HashMap<String, JobWatch> = HashMap::new();
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    watch_once(&mut jobs);
                }
            });
    });
}

fn watch_once(jobs: &mut HashMap<String, JobWatch>) {
    let sessions: Vec<_> = {
        let Ok(registry) = super::registry::registry().lock() else {
            return;
        };
        registry
            .list()
            .into_iter()
            .filter_map(|info| registry.get(&info.id))
            .collect()
    };
    let live = |id: &String| sessions.iter().any(|session| session.id() == id);
    jobs.retain(|id, _| live(id));
    for session in sessions {
        let Some(shell) = session.shell_pid() else {
            continue;
        };
        let finished = jobs.entry(session.id().to_string()).or_default().observe(
            session.foreground_pgrp(),
            shell,
            Instant::now(),
            |group| super::session::process_name(group).unwrap_or_else(|| "command".to_string()),
            is_stopped,
        );
        if let Some((job, ran)) = finished {
            // Who hears it (alerts on, terminal access, password proved) is
            // decided per phone there.
            crate::device_links::webrtc::terminal_bridge::announce_finished(
                session.id(),
                &job,
                ran.as_secs(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_job_finishing_is_reported_once_and_a_short_one_never() {
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        let mut watch = JobWatch::default();
        let name = |group: i32| format!("job{group}");
        let running = |_: i32| false;
        // At the prompt.
        assert_eq!(watch.observe(Some(100), 100, at(0), name, running), None);
        // `cargo build` (group 200) runs for 30 s.
        assert_eq!(watch.observe(Some(200), 100, at(1), name, running), None);
        assert_eq!(watch.observe(Some(200), 100, at(20), name, running), None);
        assert_eq!(
            watch.observe(None, 100, at(25), name, running),
            None,
            "unknown changes nothing"
        );
        assert_eq!(
            watch.observe(Some(100), 100, at(31), name, running),
            Some(("job200".to_string(), Duration::from_secs(30)))
        );
        assert_eq!(
            watch.observe(Some(100), 100, at(32), name, running),
            None,
            "reported once"
        );
        // `ls` (group 300) for 2 s: no alert.
        assert_eq!(watch.observe(Some(300), 100, at(40), name, running), None);
        assert_eq!(watch.observe(Some(100), 100, at(42), name, running), None);
    }

    #[test]
    fn a_new_job_in_front_restarts_the_clock() {
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        let mut watch = JobWatch::default();
        let name = |group: i32| format!("job{group}");
        let running = |_: i32| false;
        watch.observe(Some(200), 100, at(0), name, running);
        // A second job takes the foreground at 20 s and runs 5 s.
        watch.observe(Some(201), 100, at(20), name, running);
        assert_eq!(watch.observe(Some(100), 100, at(25), name, running), None);
    }

    #[test]
    fn editors_pagers_and_stopped_jobs_are_not_finished_commands() {
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        let mut watch = JobWatch::default();
        // `vim` for a minute, then back at the prompt: not news.
        watch.observe(Some(200), 100, at(0), |_| "vim".to_string(), |_| false);
        assert_eq!(
            watch.observe(Some(100), 100, at(60), |_| unreachable!(), |_| false),
            None
        );
        // `cargo build` for 30 s, then Ctrl-Z: stopped, not finished.
        watch.observe(Some(300), 100, at(70), |_| "cargo".to_string(), |_| false);
        assert_eq!(
            watch.observe(Some(100), 100, at(100), |_| unreachable!(), |_| true),
            None
        );
    }
}
