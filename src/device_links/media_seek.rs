//! Seeking a player: the rules both directions and every control share.
//! Positions are milliseconds on the wire (`pos`, `length`, `SetPosition`);
//! only MPRIS itself speaks microseconds, and only [`plan`] and the D-Bus
//! call in `mpris.rs` cross that line.

use std::time::{Duration, Instant};

/// A seek just made holds the bar this long: a player applies it
/// asynchronously, so a report sent meanwhile still has the old position.
pub(crate) const HOLD: Duration = Duration::from_millis(1500);
/// While a bar is dragged, at most five seeks a second go out.
pub(crate) const DRAG_EVERY: Duration = Duration::from_millis(200);

/// `position_ms` kept inside the track (no upper bound when the length is unknown).
pub(crate) fn clamp(position_ms: i64, length_ms: Option<i64>) -> i64 {
    match length_ms.filter(|length| *length > 0) {
        Some(length) => position_ms.clamp(0, length),
        None => position_ms.max(0),
    }
}

/// Why a bar cannot seek, in words for the person; `None` when it can.
pub(crate) fn blocked(can_seek: bool, length_ms: Option<i64>) -> Option<&'static str> {
    if length_ms.is_none_or(|length| length <= 0) {
        Some("Can't seek: this has no fixed length (a live stream)")
    } else if !can_seek {
        Some("Can't seek: this player does not allow it")
    } else {
        None
    }
}

/// Where a playing track is now: the last report plus the time since.
pub(crate) fn now_at(
    position_ms: i64,
    reported: Instant,
    now: Instant,
    playing: bool,
    length_ms: Option<i64>,
) -> i64 {
    let elapsed = if playing {
        now.saturating_duration_since(reported).as_millis() as i64
    } else {
        0
    };
    clamp(position_ms.saturating_add(elapsed), length_ms)
}

/// A seek to `target_ms` made at `at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hold {
    pub target_ms: i64,
    pub at: Instant,
}

impl Hold {
    /// Whether a report made at `now` is the old position of a seek in flight.
    pub(crate) fn ignores(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) < HOLD
    }
}

/// Sends a dragged bar's values at most every [`DRAG_EVERY`].
#[derive(Debug, Default)]
pub(crate) struct Throttle {
    last: Option<Instant>,
}

impl Throttle {
    /// Whether a send is allowed at `now` (and counts as made).
    pub(crate) fn due(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < DRAG_EVERY)
        {
            return false;
        }
        self.last = Some(now);
        true
    }

    /// A release always goes out.
    pub(crate) fn force(&mut self, now: Instant) {
        self.last = Some(now);
    }
}

/// How to move a desk player to `position_ms`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    /// `SetPosition(track, µs)`.
    Absolute { track: String, micros: i64 },
    /// `Seek(µs)`, relative: for a player with no usable track id (some
    /// browsers) or one that does not take SetPosition.
    Relative { micros: i64 },
}

pub(crate) fn plan(track: Option<&str>, position_ms: i64, current_ms: i64, length_ms: i64) -> Plan {
    let target = clamp(position_ms, Some(length_ms));
    match track.filter(|track| {
        track.starts_with('/') && *track != "/org/mpris/MediaPlayer2/TrackList/NoTrack"
    }) {
        Some(track) => Plan::Absolute {
            track: track.to_string(),
            micros: target.saturating_mul(1000),
        },
        None => Plan::Relative {
            micros: (target - current_ms).saturating_mul(1000),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_stays_inside_the_track() {
        assert_eq!(clamp(-5, Some(1000)), 0);
        assert_eq!(clamp(5000, Some(1000)), 1000);
        assert_eq!(clamp(5000, None), 5000);
        assert_eq!(clamp(5000, Some(0)), 5000);
    }

    #[test]
    fn a_stream_or_a_refusing_player_says_why() {
        assert!(blocked(true, Some(0)).unwrap().contains("live"));
        assert!(blocked(true, None).is_some());
        assert!(blocked(false, Some(9)).unwrap().contains("player"));
        assert!(blocked(true, Some(9)).is_none());
    }

    #[test]
    fn a_playing_track_moves_on_and_stops_at_the_end() {
        let t = Instant::now();
        let later = t + Duration::from_secs(3);
        assert_eq!(now_at(1000, t, later, true, Some(60_000)), 4000);
        assert_eq!(now_at(1000, t, later, false, Some(60_000)), 1000);
        assert_eq!(now_at(59_000, t, later, true, Some(60_000)), 60_000);
    }

    #[test]
    fn a_report_inside_the_hold_is_the_old_position() {
        let t = Instant::now();
        let hold = Hold {
            target_ms: 90_000,
            at: t,
        };
        assert!(hold.ignores(t + Duration::from_millis(900)));
        assert!(!hold.ignores(t + Duration::from_millis(1600)));
    }

    #[test]
    fn a_drag_sends_five_a_second() {
        let t = Instant::now();
        let mut throttle = Throttle::default();
        assert!(throttle.due(t));
        assert!(!throttle.due(t + Duration::from_millis(50)));
        assert!(!throttle.due(t + Duration::from_millis(150)));
        assert!(throttle.due(t + Duration::from_millis(210)));
    }

    #[test]
    fn a_track_id_sets_the_position_in_microseconds_else_the_offset_is_sent() {
        assert_eq!(
            plan(Some("/org/chromium/x/1"), 90_000, 10_000, 200_000),
            Plan::Absolute {
                track: "/org/chromium/x/1".into(),
                micros: 90_000_000
            }
        );
        // Past the end: the end. Spotify's string ids and NoTrack are not paths a call can take.
        assert_eq!(
            plan(Some("/org/chromium/x/1"), 900_000, 0, 200_000),
            Plan::Absolute {
                track: "/org/chromium/x/1".into(),
                micros: 200_000_000
            }
        );
        assert_eq!(
            plan(Some("spotify:track:1"), 90_000, 100_000, 200_000),
            Plan::Relative {
                micros: -10_000_000
            }
        );
        assert_eq!(
            plan(None, 30_000, 10_000, 200_000),
            Plan::Relative { micros: 20_000_000 }
        );
    }
}
