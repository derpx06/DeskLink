//! The InputCapture portal session ([`Portal`]): its D-Bus calls and signals,
//! arming the barrier with backoffs, sleep detection and the availability
//! check.
//!
//! Sibling edges: `machine` (zones and barriers). Outside: gio D-Bus, `eis`.

use super::*;

/// A portal signal, queued by its handler for the worker.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Signal {
    Activated { id: u32, cursor: (f64, f64) },
    Deactivated,
    Disabled,
    ZonesChanged,
    Closed,
}

/// The live InputCapture session. Dropping it closes it.
pub(super) struct Portal {
    /// Told to this session's EIS reader, so a late word from an older one
    /// is ignored.
    pub(super) generation: u64,
    connection: gio::DBusConnection,
    handle: String,
    pub(super) signals: std::rc::Rc<std::cell::RefCell<VecDeque<Signal>>>,
    subscriptions: Vec<gio::SignalSubscriptionId>,
    pub(super) enabled: bool,
    /// When the barrier was last (re)armed, for [`REARMS_PER_MINUTE`].
    pub(super) armed_at: VecDeque<Instant>,
}

impl Portal {
    /// CreateSession (the consent dialog), and its signals and EIS socket.
    pub(super) fn open(
        context: &glib::MainContext,
        connection: &gio::DBusConnection,
    ) -> Result<Self, String> {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let created = request(
            context,
            connection,
            &cancel,
            PORTAL_PATH,
            INTERFACE,
            "CreateSession",
            tuple([
                "".to_variant(),
                options(|dict| {
                    dict.insert("handle_token", token());
                    dict.insert("session_handle_token", token());
                    // Keyboard and pointer.
                    dict.insert("capabilities", 3u32);
                }),
            ]),
        )?;
        let handle = lookup::<String>(&created, "session_handle")?
            .ok_or_else(|| "This computer did not start input capture".to_string())?;
        let granted = lookup::<u32>(&created, "capabilities")?.unwrap_or(0);
        let signals = std::rc::Rc::new(std::cell::RefCell::new(VecDeque::new()));
        static GENERATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let generation = GENERATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut portal = Self {
            generation,
            connection: connection.clone(),
            handle,
            signals,
            subscriptions: Vec::new(),
            enabled: false,
            armed_at: VecDeque::new(),
        };
        if granted & 3 != 3 {
            return Err("This computer allowed only part of the input".to_string());
        }
        portal.subscribe();
        let socket = portal.connect_to_eis()?;
        eis::spawn_receiver(socket, "DeskLink", move |captured| {
            if matches!(captured, Captured::Closed) {
                send_closed(generation)
            } else {
                send_command(Command::Captured(generation, captured))
            }
        })?;
        Ok(portal)
    }

    fn subscribe(&mut self) {
        let queue = std::rc::Rc::clone(&self.signals);
        let handle = self.handle.clone();
        let id = self.connection.signal_subscribe(
            Some(PORTAL_BUS),
            Some(INTERFACE),
            None,
            Some(PORTAL_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, member, parameters| {
                crate::guard("glide signal", (), || {
                    if parameters.child_value(0).str() != Some(handle.as_str()) {
                        return;
                    }
                    let values = parameters.child_value(1);
                    let signal = match member {
                        "Activated" => {
                            let id = lookup::<u32>(&values, "activation_id").ok().flatten();
                            let cursor = lookup::<(f64, f64)>(&values, "cursor_position")
                                .ok()
                                .flatten();
                            match (id, cursor) {
                                (Some(id), Some(cursor)) => Signal::Activated { id, cursor },
                                _ => return,
                            }
                        }
                        "Deactivated" => Signal::Deactivated,
                        "Disabled" => Signal::Disabled,
                        "ZonesChanged" => Signal::ZonesChanged,
                        _ => return,
                    };
                    let mut queue = queue.borrow_mut();
                    if queue.len() < PENDING_MAX {
                        queue.push_back(signal);
                    }
                })
            },
        );
        self.subscriptions.push(id);
        let queue = std::rc::Rc::clone(&self.signals);
        let id = self.connection.signal_subscribe(
            Some(PORTAL_BUS),
            Some(SESSION_INTERFACE),
            Some("Closed"),
            Some(&self.handle),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, _| queue.borrow_mut().push_back(Signal::Closed),
        );
        self.subscriptions.push(id);
    }

    fn call(&self, method: &str, parameters: glib::Variant) -> Result<glib::Variant, String> {
        self.connection
            .call_sync(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                INTERFACE,
                method,
                Some(&parameters),
                None,
                gio::DBusCallFlags::NONE,
                CALL_TIMEOUT_MS,
                None::<&gio::Cancellable>,
            )
            .map_err(|error| format!("Input capture {method} failed: {error}"))
    }

    fn connect_to_eis(&self) -> Result<std::os::fd::OwnedFd, String> {
        let (reply, list) = self
            .connection
            .call_with_unix_fd_list_sync(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                INTERFACE,
                "ConnectToEIS",
                Some(&tuple([object_path(&self.handle)?, options(|_| {})])),
                None,
                gio::DBusCallFlags::NONE,
                CALL_TIMEOUT_MS * 5,
                None::<&gio::UnixFDList>,
                None::<&gio::Cancellable>,
            )
            .map_err(|error| format!("Input capture ConnectToEIS failed: {error}"))?;
        let index = pipewire_fd_index(&reply)?;
        let raw = list
            .ok_or_else(|| "This computer sent no input socket".to_string())?
            .get(index)
            .map_err(|error| format!("Could not receive the input socket: {error}"))?;
        // SAFETY: `get` returned a fresh descriptor this side now owns.
        Ok(unsafe { std::os::fd::FromRawFd::from_raw_fd(raw) })
    }

    /// GetZones → SetPointerBarriers on `edge` → Enable. Zones are read
    /// afresh every time: they change with monitors, the lid and the scale.
    pub(super) fn arm(
        &mut self,
        context: &glib::MainContext,
        edge: Edge,
    ) -> Result<Barrier, ArmError> {
        let now = Instant::now();
        self.armed_at
            .retain(|at| now.saturating_duration_since(*at) < MINUTE);
        self.armed_at.push_back(now);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let session = object_path(&self.handle).map_err(ArmError::Gone)?;
        let zones = request(
            context,
            &self.connection,
            &cancel,
            PORTAL_PATH,
            INTERFACE,
            "GetZones",
            tuple([
                session.clone(),
                options(|dict| dict.insert("handle_token", token())),
            ]),
        )
        .map_err(zones_error)?;
        let zone_set = lookup::<u32>(&zones, "zone_set")
            .map_err(ArmError::Retry)?
            .unwrap_or(0);
        let zones: Vec<Zone> = lookup::<Vec<(u32, u32, i32, i32)>>(&zones, "zones")
            .map_err(ArmError::Retry)?
            .unwrap_or_default()
            .into_iter()
            .take(64)
            .map(|(width, height, x, y)| Zone {
                x,
                y,
                width: width.min(1 << 20) as i32,
                height: height.min(1 << 20) as i32,
            })
            .collect();
        let barrier = Barrier::on(edge, &zones)
            .ok_or_else(|| ArmError::Retry("This screen has no edge to glide from".to_string()))?;
        let position = glib::VariantDict::new(None);
        position.insert("barrier_id", 1u32);
        position.insert("position", (barrier.x1, barrier.y1, barrier.x2, barrier.y2));
        let set = request(
            context,
            &self.connection,
            &cancel,
            PORTAL_PATH,
            INTERFACE,
            "SetPointerBarriers",
            tuple([
                session.clone(),
                options(|dict| dict.insert("handle_token", token())),
                glib::Variant::array_from_iter_with_type(
                    glib::VariantTy::VARDICT,
                    [position.end()],
                ),
                zone_set.to_variant(),
            ]),
        )
        .map_err(ArmError::Retry)?;
        // Most often the zones changed between GetZones and here ("State out
        // of date"): the next try reads them again.
        if lookup::<Vec<u32>>(&set, "failed_barriers")
            .map_err(ArmError::Retry)?
            .is_some_and(|failed| !failed.is_empty())
        {
            return Err(ArmError::Retry(
                "This computer refused that edge".to_string(),
            ));
        }
        self.call("Enable", tuple([session, options(|_| {})]))
            .map_err(ArmError::Retry)?;
        self.enabled = true;
        Ok(barrier)
    }

    pub(super) fn disable(&mut self) {
        if let Ok(session) = object_path(&self.handle) {
            let _ = self.call("Disable", tuple([session, options(|_| {})]));
        }
        self.enabled = false;
    }

    /// Ends the capture; false when the desktop refused (the caller then
    /// disables the session, which ends it too).
    pub(super) fn release(&self, activation: u32, x: f64, y: f64) -> bool {
        let Ok(session) = object_path(&self.handle) else {
            return false;
        };
        if let Err(error) = self.call(
            "Release",
            tuple([
                session,
                options(|dict| {
                    dict.insert("activation_id", activation);
                    dict.insert("cursor_position", (x, y));
                }),
            ]),
        ) {
            log_limited("release", &error);
            return false;
        }
        true
    }
}

impl Drop for Portal {
    fn drop(&mut self) {
        for id in self.subscriptions.drain(..) {
            self.connection.signal_unsubscribe(id);
        }
        let _ = self.connection.call_sync(
            Some(PORTAL_BUS),
            &self.handle,
            SESSION_INTERFACE,
            "Close",
            None,
            None,
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            None::<&gio::Cancellable>,
        );
    }
}

pub(super) const MINUTE: Duration = Duration::from_secs(60);

/// Why an arming failed, and so what follows.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ArmError {
    /// The desktop no longer knows the session: GNOME drops its half when the
    /// screen locks and the portal's half lingers, answering every GetZones
    /// with "denied". Only a new session (a new dialog) can work.
    Gone(String),
    /// Anything else (zones changed mid-way, no edge yet, Enable refused):
    /// tried again, never given up.
    Retry(String),
}

/// GetZones is the first thing every arming asks, so it is where a session
/// the desktop forgot shows up: the portal answered and refused it (its
/// Response says denied or cancelled), or said the session is not there
/// (`AccessDenied: Invalid session`, "Object does not exist"). Anything
/// else (the portal restarting: `ServiceUnknown`, `NoReply`, a cancelled or
/// timed-out call) is the bus, not the session, and is only retried.
pub(super) fn zones_error(error: String) -> ArmError {
    let refused = error.starts_with("Desktop permission request")
        && (error.ends_with("was denied") || error.ends_with("was cancelled"));
    let missing = [
        "AccessDenied",
        "Invalid session",
        "UnknownObject",
        "does not exist",
    ]
    .iter()
    .any(|sign| error.contains(sign));
    if refused || missing {
        ArmError::Gone(error)
    } else {
        ArmError::Retry(error)
    }
}

/// When the barrier may next be armed, given the armings of the last minute.
pub(super) fn next_arm_at(armed: &VecDeque<Instant>, now: Instant) -> Instant {
    armed
        .iter()
        .rev()
        .nth(REARMS_PER_MINUTE - 1)
        .map_or(now, |at| (*at + MINUTE).max(now))
}

/// How long after the `failures`-th failed arming in a row to try again: at
/// once (the zones are read afresh), then 1, 5, 15, 30 s, then every minute.
/// Before, three failures turned glide off for the rest of the run.
pub(super) fn arm_retry_after(failures: u32) -> Duration {
    const STEPS: [u64; 6] = [0, 1, 5, 15, 30, 60];
    let step = (failures.max(1) as usize - 1).min(STEPS.len() - 1);
    Duration::from_secs(STEPS[step])
}

/// Failed armings in a row before the window says so (it keeps trying).
pub(super) const ARM_RETRIES: u32 = 3;

/// Time spent suspended since boot: `CLOCK_BOOTTIME` counts a suspend and
/// `CLOCK_MONOTONIC` does not, while a wall-clock step (NTP, a manual change)
/// moves neither.
pub(super) fn suspended_so_far() -> Duration {
    let read = |clock| {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `time` is a valid, writable timespec for the call.
        if unsafe { libc::clock_gettime(clock, &mut time) } == 0 {
            Duration::new(
                time.tv_sec.max(0) as u64,
                time.tv_nsec.clamp(0, 999_999_999) as u32,
            )
        } else {
            Duration::ZERO
        }
    };
    let monotonic = read(libc::CLOCK_MONOTONIC);
    read(libc::CLOCK_BOOTTIME).saturating_sub(monotonic)
}

/// The computer slept since the last look: its suspended time grew.
pub(super) fn slept(suspended_before: Duration, suspended_now: Duration) -> bool {
    suspended_now.saturating_sub(suspended_before) > Duration::from_secs(5)
}

/// How long to wait before arming again after the desktop disabled the edge
/// `disables` times this minute: at once for the first two (a monitor change
/// sends Disabled and ZonesChanged), then backing off like a failed arming,
/// so a desktop that keeps disabling it is not re-armed six times a minute
/// for ever.
pub(super) fn rearm_delay(disables: usize) -> Duration {
    if disables < 3 {
        Duration::ZERO
    } else {
        arm_retry_after(disables as u32 - 1)
    }
}

/// Whether this session can glide: Wayland, and a portal with InputCapture
/// for pointer and keyboard.
pub(super) fn check_availability(connection: &gio::DBusConnection) -> Availability {
    // The window's own backend: X11 (or forced to it) cannot capture.
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        && !std::env::var("GDK_BACKEND").is_ok_and(|backend| backend.starts_with("x11"));
    if !wayland {
        return Availability::Unavailable("Glide needs a Wayland session; this one is X11");
    }
    let supported = connection
        .call_sync(
            Some(PORTAL_BUS),
            PORTAL_PATH,
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&(INTERFACE, "SupportedCapabilities").to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            None::<&gio::Cancellable>,
        )
        .ok()
        .and_then(|reply| reply.child_value(0).as_variant())
        .and_then(|value| value.get::<u32>());
    match supported {
        Some(capabilities) if capabilities & 3 == 3 => Availability::Available,
        _ => Availability::Unavailable(
            "This desktop does not offer input capture (it needs GNOME 45 or later, or \
             KDE Plasma 6.1 or later)",
        ),
    }
}

/// Plain words for a portal failure.
pub(super) fn plain(error: &str) -> String {
    if error.contains("cancelled") || error.contains("denied") {
        Why::Declined.words().to_string()
    } else if error.starts_with("Input capture") || error.contains("portal") {
        "This desktop's input capture did not answer".to_string()
    } else {
        error.to_string()
    }
}
