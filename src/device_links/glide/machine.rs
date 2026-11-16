//! The glide state machine ([`Glide`]): barrier zones on the chosen edge and
//! what each captured event becomes (`Out`, with the packets to send). No
//! portal, no D-Bus, no threads; the keymap lookup is passed in.
//!
//! Sibling edges: `input_packets` (modifiers, button codes, packet builders).

use super::*;

/// A screen area, in the portal's zone coordinates (logical pixels).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Zone {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) width: i32,
    pub(super) height: i32,
}

/// The barrier on the chosen edge, and the zone it borders.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Barrier {
    pub(super) edge: Edge,
    pub(super) x1: i32,
    pub(super) y1: i32,
    pub(super) x2: i32,
    pub(super) y2: i32,
    zone: Zone,
}

impl Barrier {
    /// On the outer side of the zone furthest towards `edge` (as the
    /// portal requires: on the union's boundary, inside one zone). Of two
    /// screens that reach equally far, the longer edge wins, whatever order
    /// the portal lists them in, so the edge does not move between re-arms.
    pub(super) fn on(edge: Edge, zones: &[Zone]) -> Option<Self> {
        use std::cmp::Reverse;
        let zone = *match edge {
            Edge::Left => zones
                .iter()
                .min_by_key(|zone| (zone.x, Reverse(zone.height))),
            Edge::Right => zones
                .iter()
                .max_by_key(|zone| (zone.x + zone.width, zone.height)),
            Edge::Top => zones
                .iter()
                .min_by_key(|zone| (zone.y, Reverse(zone.width))),
            Edge::Bottom => zones
                .iter()
                .max_by_key(|zone| (zone.y + zone.height, zone.width)),
        }?;
        if zone.width <= 0 || zone.height <= 0 {
            return None;
        }
        let (right, bottom) = (zone.x + zone.width, zone.y + zone.height);
        let (x1, y1, x2, y2) = match edge {
            Edge::Left => (zone.x, zone.y, zone.x, bottom - 1),
            Edge::Right => (right, zone.y, right, bottom - 1),
            Edge::Top => (zone.x, zone.y, right - 1, zone.y),
            Edge::Bottom => (zone.x, bottom, right - 1, bottom),
        };
        Some(Self {
            edge,
            x1,
            y1,
            x2,
            y2,
            zone,
        })
    }

    fn length(&self) -> i64 {
        i64::from(if self.edge.vertical() {
            self.zone.height
        } else {
            self.zone.width
        })
    }

    /// G1: where the pointer crossed, relative to the barrier's start.
    pub(super) fn enter_packet(&self, cursor: (f64, f64)) -> NetworkPacket {
        let along = |offset: f64| (offset.round() as i64).clamp(0, self.length());
        let (dx, dy) = (cursor.0 - f64::from(self.x1), cursor.1 - f64::from(self.y1));
        let (dx, dy) = if self.edge.vertical() {
            (dx.round() as i64, along(dy))
        } else {
            (along(dx), dy.round() as i64)
        };
        let mut packet = NetworkPacket::new(PACKET_TYPE_SHARE_INPUT_DEVICES_REQUEST);
        packet.set("exitEdge", self.edge.qt());
        packet.set("deltax", dx);
        packet.set("deltay", dy);
        packet.set("edgeLength", self.length());
        packet
    }

    /// A point just inside the zone, at `(x, y)` along the edge.
    fn inside(&self, x: f64, y: f64) -> (f64, f64) {
        /// Pixels inside the edge, so a jitter does not cross back at once.
        const INSET: i32 = 2;
        let zone = self.zone;
        let clamp = |value: f64, low: i32, high: i32| value.clamp(f64::from(low), f64::from(high));
        let x = clamp(x, zone.x, zone.x + zone.width - 1);
        let y = clamp(y, zone.y, zone.y + zone.height - 1);
        match self.edge {
            Edge::Left => (f64::from(zone.x + INSET), y),
            Edge::Right => (f64::from(zone.x + zone.width - 1 - INSET), y),
            Edge::Top => (x, f64::from(zone.y + INSET)),
            Edge::Bottom => (x, f64::from(zone.y + zone.height - 1 - INSET)),
        }
    }

    /// G3: the barrier's start plus the phone's release deltas.
    fn release_point(&self, dx: f64, dy: f64) -> (f64, f64) {
        self.inside(f64::from(self.x1) + dx, f64::from(self.y1) + dy)
    }
}

/// What the state machine asks the worker to do.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Out {
    /// A packet for the phone.
    Send(NetworkPacket),
    /// The pointer just entered the phone: its clipboard follows (G6).
    Clipboard,
    /// End the capture, the pointer at `(x, y)`.
    Release { activation: u32, x: f64, y: f64 },
    /// Gliding started or ended (the indicator).
    Gliding(bool),
    /// The phone did not accept the pointer: tell the person (once).
    Refused,
}

/// One activation of the barrier, start to release (unit-tested).
#[derive(Debug)]
pub(super) struct Glide {
    barrier: Barrier,
    /// The portal's activation id while the pointer is on the phone.
    pub(super) active: Option<u32>,
    /// Where the pointer crossed: Esc+Super brings it back there.
    crossed: (f64, f64),
    /// Captured before the Activated signal arrived.
    pub(super) pending: VecDeque<(Instant, Captured)>,
    mods: Mods,
    /// Motion not sent yet, and when motion was last sent.
    pub(super) motion: (f64, f64),
    motion_sent: Option<Instant>,
    pub(super) dropped: u64,
    /// The left button is down on the phone (a drag): every way out lets it
    /// go first, so the phone is never left holding a touch.
    pub(super) left_down: bool,
    /// The pointer crossed and the phone has not accepted yet (since when):
    /// nothing is streamed, input is held.
    entering: Option<Instant>,
    /// Input held while entering, at most [`HELD_MAX`].
    pub(super) held: VecDeque<Captured>,
    /// Production: the phone must accept before anything streams. Older
    /// tests of the streaming itself skip it.
    pub(super) accept_needed: bool,
    /// A refusal: crossings are released at once until then.
    pub(super) backoff_until: Option<Instant>,
    /// The person was told of a refusal and no accept has come since.
    pub(super) told: bool,
}

impl Glide {
    pub(super) fn new(barrier: Barrier) -> Self {
        Self {
            accept_needed: true,
            ..Self::accepting(barrier)
        }
    }

    /// A glide that streams at once (tests of the streaming itself).
    pub(super) fn accepting(barrier: Barrier) -> Self {
        Self {
            entering: None,
            held: VecDeque::new(),
            accept_needed: false,
            backoff_until: None,
            told: false,
            barrier,
            active: None,
            crossed: (0.0, 0.0),
            pending: VecDeque::new(),
            mods: Mods::default(),
            motion: (0.0, 0.0),
            motion_sent: None,
            dropped: 0,
            left_down: false,
        }
    }

    /// The release of a held left button, once, as the first thing sent when
    /// the pointer leaves the phone.
    pub(super) fn let_go(&mut self) -> Option<Out> {
        std::mem::take(&mut self.left_down)
            .then(|| Out::Send(mousepad(|packet| packet.set("singlerelease", true))))
    }

    #[cfg(test)]
    pub(super) fn gliding(&self) -> bool {
        self.active.is_some()
    }

    /// The portal's Activated: tell the phone where, then what came early.
    pub(super) fn activated(
        &mut self,
        activation: u32,
        cursor: (f64, f64),
        keys: &dyn Fn(u32, Mods) -> Option<gdk::Key>,
        now: Instant,
    ) -> Vec<Out> {
        // A refusal just now: the pointer goes straight back, nothing is
        // sent, nothing streams.
        if self.backoff_until.is_some_and(|until| now < until) {
            transition("crossing released at once: this edge is resting after a refusal");
            self.pending.clear();
            let (x, y) = self.barrier.inside(cursor.0, cursor.1);
            return vec![Out::Release { activation, x, y }];
        }
        let was = self.active.replace(activation);
        transition(if was.is_some() {
            "crossing again while already on the phone"
        } else {
            "crossing: telling the phone, waiting for it to accept"
        });
        // Already on the phone (and accepted) when the portal activates again:
        // nothing new to wait for.
        let waiting = was.is_none() || self.entering.is_some();
        self.crossed = cursor;
        self.mods = Mods::default();
        self.motion = (0.0, 0.0);
        self.held.clear();
        self.entering = (self.accept_needed && waiting).then_some(now);
        let mut out = vec![Out::Send(self.barrier.enter_packet(cursor))];
        if was.is_none() {
            out.push(Out::Gliding(true));
            out.push(Out::Clipboard);
        }
        // Only what came just before the crossing belongs to it: input that
        // arrived after an earlier release and waited (a stale tail of the
        // last capture, say) is not replayed onto the phone.
        for (at, captured) in std::mem::take(&mut self.pending) {
            if now.saturating_duration_since(at) <= PENDING_FRESH {
                out.extend(self.input(captured, keys, now));
            }
        }
        out
    }

    /// The compositor ended the capture itself (lock screen, session end).
    pub(super) fn deactivated(&mut self) -> Vec<Out> {
        self.pending.clear();
        self.held.clear();
        self.entering = None;
        if self.active.take().is_none() {
            return Vec::new();
        }
        transition("back: the desktop ended the capture (lock, session end or edge change)");
        let mut out: Vec<Out> = self.let_go().into_iter().collect();
        out.push(Out::Send(Self::taken_back()));
        out.push(Out::Gliding(false));
        out
    }

    /// G4: the desk takes the pointer back (Esc+Super, link down, glide
    /// off), where it crossed.
    pub(super) fn take_back(&mut self) -> Vec<Out> {
        self.pending.clear();
        self.held.clear();
        self.entering = None;
        let Some(activation) = self.active.take() else {
            return Vec::new();
        };
        transition("back: the desk took the pointer (Esc+Super, link down or silent, Glide off, screens changed)");
        let (x, y) = self.barrier.inside(self.crossed.0, self.crossed.1);
        let mut out: Vec<Out> = self.let_go().into_iter().collect();
        out.extend([
            Out::Send(Self::taken_back()),
            Out::Release { activation, x, y },
            Out::Gliding(false),
        ]);
        out
    }

    pub(super) fn taken_back() -> NetworkPacket {
        let mut packet = NetworkPacket::new(PACKET_TYPE_SHARE_INPUT_DEVICES_REQUEST);
        packet.set("exitEdge", 0);
        packet
    }

    /// G3: the phone hands the pointer back.
    pub(super) fn phone_released(&mut self, dx: f64, dy: f64, now: Instant) -> Vec<Out> {
        self.pending.clear();
        // Before it accepted, a hand-back is a refusal: back where it crossed.
        if self.entering.is_some() {
            return self.refused(now, false);
        }
        let Some(activation) = self.active.take() else {
            return Vec::new();
        };
        transition("back: the phone handed the pointer back at the edge");
        let (x, y) = self.barrier.release_point(dx, dy);
        let mut out: Vec<Out> = self.let_go().into_iter().collect();
        out.extend([Out::Release { activation, x, y }, Out::Gliding(false)]);
        out
    }

    /// The phone accepted the pointer: what was held goes out, in order.
    pub(super) fn accepted(
        &mut self,
        keys: &dyn Fn(u32, Mods) -> Option<gdk::Key>,
        now: Instant,
    ) -> Vec<Out> {
        if self.entering.take().is_none() {
            return Vec::new();
        }
        self.told = false;
        transition("the phone accepted the pointer: streaming");
        self.mods = Mods::default();
        let mut out = Vec::new();
        for captured in std::mem::take(&mut self.held) {
            out.extend(self.input(captured, keys, now));
        }
        out.extend(self.flush_motion(now));
        out
    }

    /// The phone did not accept (it handed the pointer back, said it does not
    /// allow glide, or did not answer in time): the pointer is released where
    /// it crossed, nothing was streamed, and this edge is left alone for
    /// [`REFUSED_BACKOFF`] ([`UNANSWERED_BACKOFF`] when it never answered).
    /// `tell_phone`: the phone is told to forget the entry (it never
    /// answered).
    fn refused(&mut self, now: Instant, tell_phone: bool) -> Vec<Out> {
        self.pending.clear();
        self.held.clear();
        self.entering = None;
        self.motion = (0.0, 0.0);
        let rest = if tell_phone {
            UNANSWERED_BACKOFF
        } else {
            REFUSED_BACKOFF
        };
        self.backoff_until = Some(now + rest);
        transition(if tell_phone {
            "enter refused: the phone did not answer in time"
        } else {
            "enter refused: the phone handed it back or says Glide is not allowed"
        });
        let Some(activation) = self.active.take() else {
            return Vec::new();
        };
        let (x, y) = self.barrier.inside(self.crossed.0, self.crossed.1);
        let mut out = Vec::new();
        if tell_phone {
            out.push(Out::Send(Self::taken_back()));
        }
        out.extend([Out::Release { activation, x, y }, Out::Gliding(false)]);
        if !std::mem::replace(&mut self.told, true) {
            out.push(Out::Refused);
        }
        out
    }

    /// The phone says it no longer allows glide (a state report): a pointer
    /// on it, or on its way, comes back at once.
    pub(super) fn disallowed(&mut self, now: Instant) -> Vec<Out> {
        if self.entering.is_some() {
            return self.refused(now, false);
        }
        self.take_back()
    }

    /// Motion waiting longer than [`MOTION_EVERY`] goes out.
    pub(super) fn tick(&mut self, now: Instant) -> Vec<Out> {
        if let Some(since) = self.entering {
            // No answer in time is a refusal.
            return if now.saturating_duration_since(since) >= ENTER_WAIT {
                self.refused(now, true)
            } else {
                Vec::new()
            };
        }
        let due = self
            .motion_sent
            .is_none_or(|sent| now.duration_since(sent) >= MOTION_EVERY);
        if due {
            self.flush_motion(now)
        } else {
            Vec::new()
        }
    }

    fn flush_motion(&mut self, now: Instant) -> Vec<Out> {
        let (dx, dy) = std::mem::take(&mut self.motion);
        if dx == 0.0 && dy == 0.0 {
            return Vec::new();
        }
        self.motion_sent = Some(now);
        vec![Out::Send(mousepad(|packet| {
            packet.set("dx", dx);
            packet.set("dy", dy);
        }))]
    }

    pub(super) fn input(
        &mut self,
        captured: Captured,
        keys: &dyn Fn(u32, Mods) -> Option<gdk::Key>,
        now: Instant,
    ) -> Vec<Out> {
        if self.active.is_none() {
            if matches!(captured, Captured::Start(_) | Captured::Closed) {
                return Vec::new();
            }
            if self.pending.len() < PENDING_MAX {
                self.pending.push_back((now, captured));
            } else {
                self.dropped += 1;
            }
            return Vec::new();
        }
        if self.entering.is_some() {
            // Nothing streams before the phone accepts; Esc+Super still works.
            if let Captured::Key(code, pressed) = captured {
                if !self.mods.update(code, pressed) && pressed && code == KEY_ESC && self.mods.meta
                {
                    return self.take_back();
                }
            }
            if self.held.len() < HELD_MAX {
                self.held.push_back(captured);
            } else {
                self.dropped += 1;
            }
            return Vec::new();
        }
        if let Captured::Motion(dx, dy) = captured {
            if dx.is_finite() && dy.is_finite() {
                self.motion.0 += dx;
                self.motion.1 += dy;
            }
            return self.tick(now);
        }
        // Order matters: motion first, then the click or key.
        let mut out = self.flush_motion(now);
        match captured {
            Captured::Button(code, pressed) => {
                let key = match (code, pressed) {
                    (BTN_LEFT, true) => {
                        self.left_down = true;
                        "singlehold"
                    }
                    (BTN_LEFT, false) => {
                        self.left_down = false;
                        "singlerelease"
                    }
                    (BTN_RIGHT, true) => "rightclick",
                    (BTN_MIDDLE, true) => "middleclick",
                    (BTN_SIDE, true) => "backclick",
                    (BTN_EXTRA, true) => "forwardclick",
                    _ => return out,
                };
                out.push(Out::Send(mousepad(|packet| packet.set(key, true))));
            }
            Captured::Scroll(dx, dy) if dx.is_finite() && dy.is_finite() => {
                out.push(Out::Send(scroll(dx, dy)));
            }
            Captured::Discrete(dx, dy) => {
                // KDE's conversion: 15° per click, y inverted.
                let step = 15.0 / 120.0;
                out.push(Out::Send(scroll(
                    f64::from(dx) * step,
                    -f64::from(dy) * step,
                )));
            }
            Captured::Locked(mask) => self.mods.caps = mask & 2 != 0,
            Captured::Key(code, pressed) => {
                if self.mods.update(code, pressed) || !pressed {
                    return out;
                }
                if code == KEY_ESC && self.mods.meta {
                    out.extend(self.take_back());
                    return out;
                }
                if let Some(packet) =
                    keys(code, self.mods).and_then(|key| key_packet(key, self.mods))
                {
                    out.push(Out::Send(packet));
                }
            }
            _ => {}
        }
        out
    }
}
