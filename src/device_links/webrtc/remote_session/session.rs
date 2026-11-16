//! One device's remote session ([`RemoteSession`]): its state, the control
//! and input leases with their heartbeat and expiry, the input sequence
//! window, and what each message does to it.
//!
//! Sibling edges: `message`.

use super::*;

/// A control lease held by this desktop over the paired phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ControlLease {
    pub id: String,
    pub expires_at: i64,
}

/// A host-approved lease for input arriving from the paired phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerInputLease {
    pub id: String,
    pub owner_device_id: String,
    pub expires_at: i64,
    sequence_window: InputSequenceWindow,
}

/// Accepts a small amount of cross-channel reordering while rejecting replayed
/// input. Pointer motion and discrete actions intentionally use different
/// WebRTC data channels, so arrival order is not a reliable sequence order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct InputSequenceWindow {
    highest: i64,
    seen: HashSet<i64>,
}

impl InputSequenceWindow {
    const WIDTH: i64 = 128;

    fn accept(&mut self, sequence: i64) -> bool {
        if sequence <= 0 {
            return false;
        }
        if sequence > self.highest {
            self.highest = sequence;
            self.seen.insert(sequence);
            let oldest = self.highest.saturating_sub(Self::WIDTH - 1);
            self.seen.retain(|seen| *seen >= oldest);
            return true;
        }
        if self.highest - sequence >= Self::WIDTH {
            return false;
        }
        self.seen.insert(sequence)
    }
}

/// What this desktop is currently doing with the paired device's screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    RequestingView,
    Viewing,
    RequestingControl,
    AuthorizingPeerControl,
    PeerControlling,
    Controlling,
    PausedLocked,
    Stopped,
}

/// The desktop half of one remote session.
///
/// Sequence numbers come from here for both control messages and input packets,
/// because the peer tracks a single `lastSequence` and advances it from either.
#[derive(Debug)]
pub struct RemoteSession {
    state: SessionState,
    pub(super) remote_session_id: Option<String>,
    direction: Option<ScreenDirection>,
    lease: Option<ControlLease>,
    pub(super) peer_input_lease: Option<PeerInputLease>,
    sequence: AtomicI64,
    /// When `state` was entered, for [`Self::expire_pending`].
    state_since: Instant,
    /// The phone's screen in its own pixels, from its `screen-ready`: taps on
    /// the desktop's view are mapped into this space.
    phone_screen: Option<(i32, i32)>,
    /// Why the phone last refused control (e.g. its accessibility service is
    /// off), so the window can say so instead of "requesting" forever.
    control_denial: Option<String>,
    /// Why the phone ended the last view (`view-denied` / `screen-error`
    /// reason: "Not now" on its prompt, a declined consent), for the window.
    ended_reason: Option<String>,
    /// The last few sessions that ended: an offer for one of them is a
    /// leftover, never a new share.
    ended: std::collections::VecDeque<String>,
    /// When the phone last said `view-granted`: at once on the ask, and again
    /// when its person taps Allow (Android's own sheet comes next), so the
    /// window keeps waiting while they answer (F25).
    granted_at: Option<Instant>,
}

impl Default for RemoteSession {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteSession {
    pub fn new() -> Self {
        Self {
            state: SessionState::Idle,
            remote_session_id: None,
            direction: None,
            lease: None,
            peer_input_lease: None,
            // The peer requires `sequence > 0` and strictly increasing, so the
            // first value handed out must be 1.
            sequence: AtomicI64::new(0),
            state_since: Instant::now(),
            phone_screen: None,
            control_denial: None,
            ended_reason: None,
            ended: std::collections::VecDeque::new(),
            granted_at: None,
        }
    }

    pub fn granted_at(&self) -> Option<Instant> {
        self.granted_at
    }

    pub fn control_denial(&self) -> Option<&str> {
        self.control_denial.as_deref()
    }

    pub fn ended_reason(&self) -> Option<&str> {
        self.ended_reason.as_deref()
    }

    pub fn phone_screen(&self) -> Option<(i32, i32)> {
        self.phone_screen
    }

    pub fn set_phone_screen(&mut self, width: i32, height: i32) {
        if width > 0 && height > 0 {
            self.phone_screen = Some((width, height));
        }
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    pub fn remote_session_id(&self) -> Option<&str> {
        self.remote_session_id.as_deref()
    }

    /// Whether this session still wants the phone's screen of share `id`:
    /// a picture shown again while the lock was free may have been stopped
    /// (or replaced) meanwhile, and must then go again.
    pub fn shows_phone_screen(&self, id: &str) -> bool {
        self.remote_session_id.as_deref() == Some(id)
            && self.direction == Some(ScreenDirection::PhoneToDesktop)
            && !matches!(self.state, SessionState::Idle | SessionState::Stopped)
    }

    pub fn direction(&self) -> Option<ScreenDirection> {
        self.direction
    }

    pub fn lease(&self) -> Option<&ControlLease> {
        self.lease.as_ref()
    }

    pub fn peer_input_lease(&self) -> Option<&PeerInputLease> {
        self.peer_input_lease.as_ref()
    }

    /// Revokes a silent phone-owned input lease without ending the screen view.
    pub fn expire_peer_input_lease(&mut self, now: i64) -> bool {
        if self
            .peer_input_lease
            .as_ref()
            .is_some_and(|lease| lease.expires_at <= now)
        {
            self.peer_input_lease = None;
            if self.state == SessionState::PeerControlling {
                self.set_state(SessionState::Viewing);
            }
            true
        } else {
            false
        }
    }

    /// The next value for a control message or an input packet.
    pub fn next_sequence(&self) -> i64 {
        self.sequence.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Starts a view request, minting the session id both ends will quote.
    pub fn begin_view(&mut self, direction: ScreenDirection) -> String {
        let id = Uuid::new_v4().to_string();
        self.remote_session_id = Some(id.clone());
        self.control_denial = None;
        self.ended_reason = None;
        self.direction = Some(direction);
        self.lease = None;
        self.peer_input_lease = None;
        self.set_state(SessionState::RequestingView);
        id
    }

    /// Takes up a phone's offer of its screen: a view request under the id
    /// the phone chose. Refused while any session is under way.
    /// `Err("stale")` for an offer of a session that already ended (a
    /// leftover arriving late): it is answered with nothing.
    pub fn adopt_offer(&mut self, id: &str) -> Result<(), String> {
        if self.ended.iter().any(|ended| ended == id) {
            return Err("stale".to_string());
        }
        if !matches!(self.state, SessionState::Idle | SessionState::Stopped) {
            return Err("busy".to_string());
        }
        self.remote_session_id = Some(id.to_string());
        self.control_denial = None;
        self.direction = Some(ScreenDirection::PhoneToDesktop);
        self.lease = None;
        self.peer_input_lease = None;
        self.set_state(SessionState::RequestingView);
        Ok(())
    }

    pub fn begin_control(&mut self) -> Result<(), String> {
        if !matches!(
            self.state,
            SessionState::Viewing | SessionState::RequestingControl
        ) {
            return Err("Remote control needs an active view".to_string());
        }
        self.set_state(SessionState::RequestingControl);
        Ok(())
    }

    /// Applies a validated inbound message.
    ///
    /// A *control* denial keeps `remote_session_id` and the view, so control can
    /// be asked for again. A view denial or screen error ends the session on both
    /// sides, and messages that end a session are honoured only for the current
    /// one, so the two ends cannot fall out of step on a stale id.
    pub fn accept(&mut self, message: &ControlMessage, now: i64) -> Result<(), String> {
        let mismatched =
            self.remote_session_id.as_deref() != Some(message.remote_session_id.as_str());
        match message.kind {
            // The desktop reports its own focus; a phone claiming it is not a
            // report about anything this side can act on.
            ControlKind::InputFocus => {
                return Err("input-focus is reported by the desktop only".to_string());
            }
            // A request starts a session, and a heartbeat is checked against the
            // lease it quotes. An offer is answered by the coordinator.
            ControlKind::RequestView | ControlKind::Heartbeat | ControlKind::OfferView => {}
            // A message that *ends* a session is honoured only for the session
            // that is current. A delayed release, denial or error for one this
            // side already left must not stop the new one (and so its portal
            // capture). With nothing active there is nothing to disturb.
            ControlKind::Release
            | ControlKind::ScreenStopped
            | ControlKind::ViewDenied
            | ControlKind::ControlDenied
            | ControlKind::ScreenError => {
                if self.remote_session_id.is_some() && mismatched {
                    return Err("Remote-session id is stale".to_string());
                }
            }
            _ => {
                if mismatched {
                    return Err("Remote-session id is stale".to_string());
                }
            }
        }

        if matches!(
            message.kind,
            ControlKind::ControlGranted | ControlKind::TakeoverGranted
        ) && !matches!(
            self.state,
            SessionState::RequestingControl | SessionState::Controlling
        ) {
            return Err("Remote-control lease was not requested".to_string());
        }

        match message.kind {
            // Refused above; never applied. An offer is taken up with
            // `adopt_offer`, not here.
            ControlKind::InputFocus | ControlKind::OfferView => {}
            // Sound and capture are not the screen session.
            ControlKind::AudioRequest
            | ControlKind::AudioReady
            | ControlKind::AudioControl
            | ControlKind::AudioStop
            | ControlKind::AudioDenied
            | ControlKind::CaptureRequest
            | ControlKind::CaptureResult
            | ControlKind::CaptureCancelled => {
                return Err(
                    "Sound and capture controls are not part of the screen session".to_string(),
                );
            }
            // A request about the current desktop share; changes no state.
            ControlKind::SwitchScreen => {
                if mismatched || self.direction != Some(ScreenDirection::DesktopToPhone) {
                    return Err("Switching screens needs a desktop share in progress".to_string());
                }
            }
            ControlKind::RequestView => {
                self.remote_session_id = Some(message.remote_session_id.clone());
                self.control_denial = None;
                self.direction = message.direction;
                self.lease = None;
                self.peer_input_lease = None;
                self.set_state(SessionState::RequestingView);
            }
            ControlKind::ViewGranted | ControlKind::ScreenReady => {
                if message.kind == ControlKind::ViewGranted {
                    self.granted_at = Some(Instant::now());
                }
                self.direction = message.direction.or(self.direction);
                // The phone says what its screen is *now*: a rotation sends
                // another `screen-ready` with the new size, which moves the
                // mapping of every later tap but does not end control.
                if message.kind == ControlKind::ScreenReady {
                    if let (Some(width), Some(height)) =
                        (message.screen_width, message.screen_height)
                    {
                        self.set_phone_screen(width, height);
                    }
                }
                let already_in_control = message.kind == ControlKind::ScreenReady
                    && matches!(
                        self.state,
                        SessionState::Controlling | SessionState::RequestingControl
                    );
                if !already_in_control {
                    self.set_state(SessionState::Viewing);
                }
            }
            ControlKind::ControlGranted | ControlKind::TakeoverGranted => {
                let id = message
                    .lease_id
                    .clone()
                    .ok_or_else(|| "A granted lease needs an identifier".to_string())?;
                let expires_at = message
                    .lease_expires_at
                    .ok_or_else(|| "A granted lease needs an expiry".to_string())?;
                self.lease = Some(ControlLease { id, expires_at });
                self.peer_input_lease = None;
                self.control_denial = None;
                self.set_state(SessionState::Controlling);
            }
            ControlKind::Heartbeat => {
                if let Some(lease) = &mut self.lease {
                    lease.expires_at = now + LEASE_LIFETIME.as_millis() as i64;
                } else if let Some(lease) = &mut self.peer_input_lease {
                    if lease.expires_at <= now {
                        self.peer_input_lease = None;
                        self.set_state(SessionState::Viewing);
                        return Err("Peer input lease has expired".to_string());
                    }
                    if message.lease_id.as_deref() != Some(lease.id.as_str())
                        || message.owner_device_id.as_deref()
                            != Some(lease.owner_device_id.as_str())
                        || !lease.sequence_window.accept(message.sequence)
                    {
                        return Err("Peer input lease heartbeat is stale or mismatched".to_string());
                    }
                    lease.expires_at = now + LEASE_LIFETIME.as_millis() as i64;
                }
            }
            ControlKind::PauseLocked => {
                self.lease = None;
                self.peer_input_lease = None;
                self.set_state(SessionState::PausedLocked);
            }
            ControlKind::Resume => self.state = SessionState::Viewing,
            // A denied view or a screen error ends the session: there is no
            // view left to show. (Mapping these to `Viewing` left the desktop's
            // dialog reading "Screen connected" after the phone said no.)
            ControlKind::ViewDenied | ControlKind::ScreenError => {
                self.stop();
                self.ended_reason = Some(
                    message
                        .reason
                        .clone()
                        .unwrap_or_else(|| "The phone declined".to_string()),
                );
            }
            // Failing to grant *control* must not take a working view with it.
            ControlKind::ControlDenied => {
                // With no session there is nothing to deny control of: ignored, not
                // a reason to start "viewing" one.
                if self.remote_session_id.is_none() {
                    return Ok(());
                }
                // A grant wins over the denial of an earlier, repeated request that
                // arrives late: control already held (and not yet expired) stays held.
                if self.state == SessionState::Controlling
                    && self
                        .lease
                        .as_ref()
                        .is_some_and(|lease| lease.expires_at > now)
                {
                    return Ok(());
                }
                self.lease = None;
                self.peer_input_lease = None;
                self.control_denial = Some(
                    message
                        .reason
                        .clone()
                        .unwrap_or_else(|| "The phone refused control".to_string()),
                );
                self.set_state(SessionState::Viewing);
            }
            ControlKind::Release | ControlKind::ScreenStopped => self.stop(),
            ControlKind::RequestControl | ControlKind::TakeoverRequest => {
                // A phone may ask to control this desktop only while viewing
                // the desktop's screen. The caller must still obtain a local
                // GNOME portal grant before it creates an input lease. Asking
                // again while already controlling (the phone retries every few
                // seconds until it sees the grant) is the same request: the
                // new grant replaces the lease. Refusing it made control flap.
                if matches!(
                    self.state,
                    SessionState::Viewing
                        | SessionState::AuthorizingPeerControl
                        | SessionState::PeerControlling
                ) && self.direction == Some(ScreenDirection::DesktopToPhone)
                {
                    self.set_state(SessionState::AuthorizingPeerControl);
                } else {
                    return Err("Phone control needs an active desktop-screen session".to_string());
                }
            }
        }
        Ok(())
    }

    fn set_state(&mut self, state: SessionState) {
        if self.state != state {
            self.state_since = Instant::now();
        }
        self.state = state;
    }

    /// Ends a request the other side never answered, returning the state it was
    /// stuck in.
    ///
    /// An unanswered *view* request ends the session. An unanswered *control*
    /// request only drops back to the view it was made on, so the user can ask
    /// again instead of losing the picture.
    pub fn expire_pending(&mut self, now: Instant) -> Option<SessionState> {
        let waited = now.saturating_duration_since(self.state_since);
        let previous = self.state;
        match previous {
            SessionState::RequestingView if waited >= VIEW_REQUEST_TIMEOUT => self.stop(),
            SessionState::RequestingControl | SessionState::AuthorizingPeerControl
                if waited >= CONTROL_REQUEST_TIMEOUT =>
            {
                self.lease = None;
                self.peer_input_lease = None;
                self.set_state(SessionState::Viewing);
            }
            _ => return None,
        }
        Some(previous)
    }

    /// [`Self::stop`], only while `id` is the current session: a share
    /// that is only now ending must not end the one that replaced it.
    pub fn stop_if_current(&mut self, id: &str) -> bool {
        let current = self.remote_session_id.as_deref() == Some(id);
        if current {
            self.stop();
        }
        current
    }

    pub fn stop(&mut self) {
        self.set_state(SessionState::Stopped);
        // The next share says its screen again (`screen-ready`): until then
        // there is no picture of it to control.
        self.phone_screen = None;
        if let Some(id) = self.remote_session_id.take() {
            if self.ended.len() == 8 {
                self.ended.pop_front();
            }
            self.ended.push_back(id);
        }
        self.direction = None;
        self.lease = None;
        self.peer_input_lease = None;
        self.control_denial = None;
        self.ended_reason = None;
    }

    /// Converts a pending host authorization into a revocable peer-owned lease.
    /// Call only after the platform portal has granted keyboard/pointer access.
    pub fn grant_peer_control(
        &mut self,
        lease_id: String,
        owner_device_id: String,
        expires_at: i64,
    ) -> Result<(), String> {
        if self.state != SessionState::AuthorizingPeerControl
            || self.direction != Some(ScreenDirection::DesktopToPhone)
        {
            return Err("Phone control has no host-approved desktop-screen request".to_string());
        }
        if lease_id.is_empty() || owner_device_id.is_empty() || expires_at <= 0 {
            return Err("Peer input lease is malformed".to_string());
        }
        self.lease = None;
        self.peer_input_lease = Some(PeerInputLease {
            id: lease_id,
            owner_device_id,
            expires_at,
            sequence_window: InputSequenceWindow::default(),
        });
        self.set_state(SessionState::PeerControlling);
        Ok(())
    }

    /// Validates a phone input packet before it can reach the portal adapter.
    /// The lease sequence is advanced only after all credentials pass.
    pub fn verify_peer_input(
        &mut self,
        packet: &crate::device_links::packet::NetworkPacket,
        owner_device_id: &str,
        now: i64,
    ) -> Result<(), String> {
        if self.state != SessionState::PeerControlling {
            return Err("DeskLink has no active host-approved input lease".to_string());
        }
        let session_id = self
            .remote_session_id
            .as_deref()
            .ok_or_else(|| "DeskLink remote input has no active session".to_string())?;
        let lease = self
            .peer_input_lease
            .as_mut()
            .ok_or_else(|| "DeskLink remote input has no active lease".to_string())?;
        if lease.owner_device_id != owner_device_id {
            return Err("DeskLink remote input came from a different device".to_string());
        }
        if lease.expires_at <= now {
            self.peer_input_lease = None;
            self.set_state(SessionState::Viewing);
            return Err("DeskLink remote-input lease has expired".to_string());
        }
        if packet.get_str(FIELD_REMOTE_SESSION_ID) != Some(session_id)
            || packet.get_str(FIELD_LEASE_ID) != Some(lease.id.as_str())
        {
            return Err("DeskLink remote input session or lease mismatch".to_string());
        }
        let sequence = packet
            .get_i64(FIELD_INPUT_SEQUENCE)
            .ok_or_else(|| "DeskLink remote input has no sequence".to_string())?;
        if !lease.sequence_window.accept(sequence) {
            return Err("Replayed DeskLink remote input".to_string());
        }
        lease.expires_at = now + LEASE_LIFETIME.as_millis() as i64;
        Ok(())
    }

    /// Whether a heartbeat is due, and the lease it should quote.
    ///
    /// Only the lease owner heartbeats. When this desktop drives the phone it
    /// holds the lease, so nothing else will keep the session alive.
    pub fn heartbeat_lease(&self, now: i64) -> Option<&ControlLease> {
        if self.state != SessionState::Controlling {
            return None;
        }
        self.lease.as_ref().filter(|lease| lease.expires_at > now)
    }

    /// Stamps an outgoing remote-input packet with its lease credentials.
    ///
    /// Without these the peer drops the packet: it checks the session id, the
    /// lease id, and a strictly increasing sequence drawn from the same counter
    /// as the heartbeats.
    pub fn stamp_input(
        &self,
        packet: &mut crate::device_links::packet::NetworkPacket,
    ) -> Result<(), String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        self.stamp_input_at(packet, now)
    }

    pub fn stamp_input_at(
        &self,
        packet: &mut crate::device_links::packet::NetworkPacket,
        now: i64,
    ) -> Result<(), String> {
        if self.state != SessionState::Controlling {
            return Err("DeskLink remote control is not active".to_string());
        }
        let session_id = self
            .remote_session_id
            .as_ref()
            .ok_or_else(|| "DeskLink remote control has no session".to_string())?;
        let lease = self
            .lease
            .as_ref()
            .ok_or_else(|| "DeskLink remote control has no lease".to_string())?;
        if lease.expires_at <= now {
            return Err("DeskLink remote-control lease has expired".to_string());
        }
        packet.set(FIELD_REMOTE_SESSION_ID, session_id.clone());
        packet.set(FIELD_LEASE_ID, lease.id.clone());
        packet.set(FIELD_INPUT_SEQUENCE, self.next_sequence());
        Ok(())
    }
}
