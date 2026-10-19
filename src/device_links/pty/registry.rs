//! The set of live terminal sessions on this machine.
//!
//! This is the core the PTY host process owns. Sessions are keyed by a
//! server-generated UUID, never by PID and never by an identifier the client
//! chose: a client-chosen id collides between two clients of the same machine,
//! and a PID is both guessable and reused by the kernel.
//!
//! Sessions outlive the client that created them. Nothing here closes a session
//! because a phone went away — only an explicit close, the shell exiting, or
//! the reaper removing something already dead.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use portable_pty::CommandBuilder;

use super::session::PtySession;

/// Concurrent sessions per machine.
///
/// Generous enough that no real workflow hits it — the point is that a peer
/// cannot exhaust file descriptors or spawn shells without bound.
pub(crate) const MAX_SESSIONS: usize = 32;

/// Sessions one phone may have created and still open. Well under the global
/// cap, so a single (or compromised) phone cannot use up every slot the desktop
/// user might want for themselves.
pub(crate) const MAX_SESSIONS_PER_DEVICE: usize = 8;

/// How long a session nobody is attached to may sit without output before it
/// is closed. A shell left at a prompt for a day is almost certainly forgotten,
/// and every open session is a live shell on this machine.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// What a client needs to render a session list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionInfo {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    /// `Some` once the program has exited; the row stays until it is reaped so
    /// the client can show *why* a session ended rather than having it vanish.
    pub(crate) exit_status: Option<u32>,
}

#[derive(Default)]
pub(crate) struct TerminalRegistry {
    sessions: HashMap<String, Arc<PtySession>>,
    /// Which device created each session, for the per-device cap.
    owners: HashMap<String, String>,
    /// Which device may currently type into each session.
    ///
    /// Single writer, many viewers: everyone attached sees the same screen, but
    /// only one device's keystrokes reach the PTY. Two writers interleave
    /// characters mid-word and neither user can tell why their typing is
    /// corrupt, so the second writer is refused rather than silently merged.
    writers: HashMap<String, String>,
}

impl TerminalRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Starts a session for `owner`. `None` gives the user's login shell.
    pub(crate) fn create(
        &mut self,
        command: Option<CommandBuilder>,
        rows: u16,
        cols: u16,
        owner: &str,
    ) -> Result<SessionInfo, String> {
        // Reap first: a machine that has accumulated finished shells should not
        // refuse a new one just because nobody has listed sessions lately.
        self.reap();
        if self.sessions.len() >= MAX_SESSIONS {
            return Err(format!(
                "DeskLink already has {MAX_SESSIONS} terminal sessions open"
            ));
        }
        let owned = self
            .owners
            .values()
            .filter(|holder| *holder == owner)
            .count();
        if owned >= MAX_SESSIONS_PER_DEVICE {
            return Err(format!(
                "This device already has {MAX_SESSIONS_PER_DEVICE} terminal sessions open"
            ));
        }
        let session = Arc::new(PtySession::spawn(command, rows, cols)?);
        let info = describe(&session);
        self.owners.insert(info.id.clone(), owner.to_string());
        self.sessions.insert(info.id.clone(), session);
        Ok(info)
    }

    pub(crate) fn get(&self, id: &str) -> Option<Arc<PtySession>> {
        self.sessions.get(id).map(Arc::clone)
    }

    /// Every live session, newest-looking order left to the caller.
    pub(crate) fn list(&self) -> Vec<SessionInfo> {
        let mut sessions: Vec<_> = self.sessions.values().map(|s| describe(s)).collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        sessions
    }

    /// Ends a session deliberately. Idempotent: closing an id that is already
    /// gone is success, because a client retrying after a dropped reply must
    /// not see an error for work that already happened.
    pub(crate) fn close(&mut self, id: &str) {
        self.writers.remove(id);
        self.owners.remove(id);
        if let Some(session) = self.sessions.remove(id) {
            session.close();
        }
    }

    /// Drops sessions whose program has exited.
    ///
    /// Returns the ids removed so the caller can tell clients why a session
    /// disappeared instead of letting it silently vanish from the next list.
    ///
    /// Removing a session here does not lose its exit status for a client that
    /// is attached: an attachment holds its own `Arc<PtySession>`, so it still
    /// sees the exit and reports it.
    pub(crate) fn reap(&mut self) -> Vec<String> {
        let dead: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.exit_status().is_some())
            .map(|(id, _)| id.clone())
            .collect();
        for id in &dead {
            self.forget(id);
        }
        dead
    }

    /// Closes sessions that nobody is attached to and that have produced no
    /// output, and had no client touch them, for `idle_for`.
    ///
    /// `attached` is the set of session ids some client currently holds; the
    /// registry does not track attachments itself, the bridges do. Returns the
    /// ids closed.
    pub(crate) fn reap_idle(
        &mut self,
        attached: &HashSet<String>,
        idle_for: Duration,
    ) -> Vec<String> {
        let idle: Vec<String> = self
            .sessions
            .iter()
            .filter(|(id, session)| !attached.contains(*id) && session.idle_for() >= idle_for)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &idle {
            self.close(id);
        }
        idle
    }

    fn forget(&mut self, id: &str) {
        self.sessions.remove(id);
        self.writers.remove(id);
        self.owners.remove(id);
    }

    /// Grants `device_id` the write lease for `terminal_id`, if free or already
    /// theirs. Returns whether they may type.
    pub(crate) fn claim_writer(&mut self, terminal_id: &str, device_id: &str) -> bool {
        if !self.sessions.contains_key(terminal_id) {
            return false;
        }
        match self.writers.get(terminal_id) {
            Some(current) if current != device_id => false,
            _ => {
                self.writers
                    .insert(terminal_id.to_string(), device_id.to_string());
                true
            }
        }
    }

    /// Gives up `device_id`'s lease on one session, if it holds it. A client
    /// that detaches must not keep every other device from typing.
    pub(crate) fn release_terminal_writer(&mut self, terminal_id: &str, device_id: &str) {
        if self
            .writers
            .get(terminal_id)
            .is_some_and(|w| w == device_id)
        {
            self.writers.remove(terminal_id);
        }
    }

    pub(crate) fn is_writer(&self, terminal_id: &str, device_id: &str) -> bool {
        self.writers
            .get(terminal_id)
            .is_some_and(|w| w == device_id)
    }

    /// Drops every write lease held by a device that has gone away, so the next
    /// client to attach can type instead of inheriting a lease nobody holds.
    pub(crate) fn release_writer(&mut self, device_id: &str) {
        self.writers.retain(|_, holder| holder != device_id);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Sessions `device_id` created and still has.
    #[cfg(test)]
    pub(crate) fn owned_by(&self, device_id: &str) -> usize {
        self.owners
            .values()
            .filter(|owner| *owner == device_id)
            .count()
    }
}

fn describe(session: &PtySession) -> SessionInfo {
    let (rows, cols) = session.size();
    SessionInfo {
        id: session.id().to_string(),
        title: session.title().to_string(),
        rows,
        cols,
        exit_status: session.exit_status(),
    }
}

/// The idle limit for unattended sessions: `DESKLINK_TERMINAL_IDLE_SECS`, else
/// 24 hours. `0` closes an unattended session at the next sweep.
pub(crate) fn idle_timeout() -> Duration {
    static TIMEOUT: OnceLock<Duration> = OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        parse_idle_timeout(std::env::var("DESKLINK_TERMINAL_IDLE_SECS").ok().as_deref())
    })
}

fn parse_idle_timeout(value: Option<&str>) -> Duration {
    value
        .and_then(|seconds| seconds.trim().parse::<u64>().ok())
        .map_or(DEFAULT_IDLE_TIMEOUT, Duration::from_secs)
}

/// The process-wide registry.
///
/// Shared the same way `webrtc::screen::streams` and
/// `webrtc::remote_session::sessions` are: the producer is a data-channel
/// callback on the peer worker thread and the consumers are the GTK main loop
/// and the terminal reader threads.
pub(crate) fn registry() -> &'static Mutex<TerminalRegistry> {
    static REGISTRY: OnceLock<Mutex<TerminalRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(TerminalRegistry::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn exiting_command() -> CommandBuilder {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        command
    }

    fn sleeping_command() -> CommandBuilder {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        command
    }

    #[test]
    fn a_created_session_is_listed_and_retrievable() {
        let mut registry = TerminalRegistry::new();
        let info = registry
            .create(Some(sleeping_command()), 24, 80, "phone-1")
            .unwrap();
        assert_eq!(registry.len(), 1);
        assert_eq!(info.rows, 24);
        assert_eq!(info.cols, 80);
        assert!(registry.get(&info.id).is_some());
        assert_eq!(registry.list(), vec![info.clone()]);
        registry.close(&info.id);
        assert_eq!(registry.len(), 0);
    }

    /// Ids must be unpredictable and unique. A PID or a counter would collide
    /// across restarts and let one client address another's session by guessing.
    #[test]
    fn session_ids_are_distinct_and_not_guessable() {
        let mut registry = TerminalRegistry::new();
        let first = registry
            .create(Some(sleeping_command()), 24, 80, "phone-1")
            .unwrap();
        let second = registry
            .create(Some(sleeping_command()), 24, 80, "phone-1")
            .unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(first.id.len(), 36, "expected a UUID");
        assert!(uuid::Uuid::parse_str(&first.id).is_ok());
    }

    /// Closing twice, or closing something already reaped, is success — a
    /// client retrying after a dropped reply must not be told it failed.
    #[test]
    fn closing_is_idempotent() {
        let mut registry = TerminalRegistry::new();
        let info = registry
            .create(Some(sleeping_command()), 24, 80, "phone-1")
            .unwrap();
        registry.close(&info.id);
        registry.close(&info.id);
        registry.close("no-such-session");
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn exited_sessions_are_reaped() {
        let mut registry = TerminalRegistry::new();
        let info = registry
            .create(Some(exiting_command()), 24, 80, "phone-1")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut reaped = Vec::new();
        while Instant::now() < deadline && reaped.is_empty() {
            reaped = registry.reap();
            if reaped.is_empty() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert_eq!(reaped, vec![info.id]);
        assert_eq!(registry.len(), 0);
    }

    /// A peer must not be able to spawn shells without bound.
    #[test]
    fn session_count_is_capped() {
        let mut registry = TerminalRegistry::new();
        let mut created = Vec::new();
        // Spread across devices so it is the global cap, not the per-device
        // one, that refuses.
        for index in 0..MAX_SESSIONS {
            let owner = format!("phone-{}", index / MAX_SESSIONS_PER_DEVICE);
            created.push(
                registry
                    .create(Some(sleeping_command()), 24, 80, &owner)
                    .unwrap(),
            );
        }
        let refused = registry.create(Some(sleeping_command()), 24, 80, "phone-9");
        assert!(refused.is_err(), "expected the cap to refuse a new session");
        for info in created {
            registry.close(&info.id);
        }
    }

    /// One phone must not be able to take every slot.
    #[test]
    fn a_single_device_is_capped_below_the_global_limit() {
        let mut registry = TerminalRegistry::new();
        let mut created = Vec::new();
        for _ in 0..MAX_SESSIONS_PER_DEVICE {
            created.push(
                registry
                    .create(Some(sleeping_command()), 24, 80, "phone-1")
                    .unwrap(),
            );
        }
        assert!(registry
            .create(Some(sleeping_command()), 24, 80, "phone-1")
            .is_err());
        // Another device is unaffected, and closing frees the slot.
        let other = registry
            .create(Some(sleeping_command()), 24, 80, "phone-2")
            .unwrap();
        registry.close(&created[0].id);
        assert!(registry
            .create(Some(sleeping_command()), 24, 80, "phone-1")
            .is_ok());
        registry.close(&other.id);
    }

    #[test]
    fn idle_unattended_sessions_are_closed_but_attached_ones_are_not() {
        let mut registry = TerminalRegistry::new();
        let watched = registry
            .create(Some(sleeping_command()), 24, 80, "phone-1")
            .unwrap();
        let forgotten = registry
            .create(Some(sleeping_command()), 24, 80, "phone-1")
            .unwrap();
        let attached: HashSet<String> = [watched.id.clone()].into();

        // Recent activity keeps even an unattended session.
        assert!(registry
            .reap_idle(&attached, Duration::from_secs(3600))
            .is_empty());

        let closed = registry.reap_idle(&attached, Duration::ZERO);
        assert_eq!(closed, vec![forgotten.id]);
        assert!(registry.get(&watched.id).is_some());
        registry.close(&watched.id);
    }

    #[test]
    fn the_idle_limit_defaults_to_a_day_and_is_configurable() {
        assert_eq!(parse_idle_timeout(None), Duration::from_secs(86_400));
        assert_eq!(parse_idle_timeout(Some("nonsense")), DEFAULT_IDLE_TIMEOUT);
        assert_eq!(parse_idle_timeout(Some(" 90 ")), Duration::from_secs(90));
    }

    /// A device that detaches must stop blocking everyone else from typing.
    #[test]
    fn releasing_one_terminal_lease_leaves_the_others() {
        let mut registry = TerminalRegistry::new();
        let first = registry
            .create(Some(sleeping_command()), 24, 80, "phone-1")
            .unwrap();
        let second = registry
            .create(Some(sleeping_command()), 24, 80, "phone-1")
            .unwrap();
        assert!(registry.claim_writer(&first.id, "phone-1"));
        assert!(registry.claim_writer(&second.id, "phone-1"));

        // Somebody else's release does nothing.
        registry.release_terminal_writer(&first.id, "phone-2");
        assert!(registry.is_writer(&first.id, "phone-1"));

        registry.release_terminal_writer(&first.id, "phone-1");
        assert!(registry.claim_writer(&first.id, "phone-2"));
        assert!(registry.is_writer(&second.id, "phone-1"));
        registry.close(&first.id);
        registry.close(&second.id);
    }
}
