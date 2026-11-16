//! Tests of glide: the state machine's edges, keys and hand-back, the phone's
//! reports, the portal's backoffs and the worker's decisions, without a portal.

use super::*;

#[test]
fn glide_packets_fit_one_wire_message() {
    let barrier = Barrier::on(Edge::Right, &screen()).unwrap();
    assert!(crate::device_links::webrtc::fits_on_wire(
        &barrier.enter_packet((f64::MAX, f64::MAX))
    ));
    let quotes = "\"".repeat(MAX_CLIPBOARD_BYTES);
    assert!(clipboard_packet(quotes)
        .is_some_and(|packet| { crate::device_links::webrtc::fits_on_wire(&packet) }));
    let controls = "\u{1}".repeat(MAX_CLIPBOARD_BYTES);
    // Escaped six times over: does not fit, so it is not sent.
    assert!(clipboard_packet(controls).is_none());
}

fn screen() -> Vec<Zone> {
    vec![Zone {
        x: 0,
        y: 0,
        width: 1600,
        height: 1000,
    }]
}

fn keys(code: u32, mods: Mods) -> Option<gdk::Key> {
    match (code, mods.shift) {
        (30, false) => Some(gdk::Key::a),
        (30, true) => Some(gdk::Key::A),
        (28, _) => Some(gdk::Key::Return),
        (1, _) => Some(gdk::Key::Escape),
        _ => None,
    }
}

fn sent(outs: &[Out]) -> Vec<serde_json::Value> {
    outs.iter()
        .filter_map(|out| match out {
            Out::Send(packet) => Some(serde_json::Value::Object(packet.body.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn barriers_sit_on_the_outer_edge_like_the_portal_requires() {
    let right = Barrier::on(Edge::Right, &screen()).unwrap();
    assert_eq!(
        (right.x1, right.y1, right.x2, right.y2),
        (1600, 0, 1600, 999)
    );
    let left = Barrier::on(Edge::Left, &screen()).unwrap();
    assert_eq!((left.x1, left.y1, left.x2, left.y2), (0, 0, 0, 999));
    let top = Barrier::on(Edge::Top, &screen()).unwrap();
    assert_eq!((top.x1, top.y1, top.x2, top.y2), (0, 0, 1599, 0));
    let bottom = Barrier::on(Edge::Bottom, &screen()).unwrap();
    assert_eq!(
        (bottom.x1, bottom.y1, bottom.x2, bottom.y2),
        (0, 1000, 1599, 1000)
    );
    // Two screens side by side: the right edge is the second one's.
    let two = [
        screen()[0],
        Zone {
            x: 1600,
            y: 0,
            width: 1920,
            height: 1080,
        },
    ];
    assert_eq!(Barrier::on(Edge::Right, &two).unwrap().x1, 3520);
    assert_eq!(Barrier::on(Edge::Left, &[]), None);
}

/// The layouts the zones change between (each change re-arms on the new
/// zones): an external monitor as primary with the laptop panel beside it
/// at a fractional scale, the lid closed, a monitor left of the primary
/// (negative x), and two screens stacked with the same right edge.
#[test]
fn the_edge_follows_the_layout_after_monitor_changes() {
    let zone = |x, y, width, height| Zone {
        x,
        y,
        width,
        height,
    };
    let line = |barrier: Barrier| (barrier.x1, barrier.y1, barrier.x2, barrier.y2);
    // External 2560x1440 primary, the laptop's 1920x1200 panel at 150 %.
    let docked = [zone(0, 0, 2560, 1440), zone(2560, 320, 1280, 800)];
    assert_eq!(
        line(Barrier::on(Edge::Right, &docked).unwrap()),
        (3840, 320, 3840, 1119)
    );
    // The lid closed: only the external monitor is left.
    assert_eq!(
        line(Barrier::on(Edge::Right, &docked[..1]).unwrap()),
        (2560, 0, 2560, 1439)
    );
    // A monitor left of the primary.
    let left_of = [zone(-2560, 0, 2560, 1440), zone(0, 0, 1536, 960)];
    assert_eq!(
        line(Barrier::on(Edge::Right, &left_of).unwrap()),
        (1536, 0, 1536, 959)
    );
    assert_eq!(
        line(Barrier::on(Edge::Left, &left_of).unwrap()),
        (-2560, 0, -2560, 1439)
    );
    // Stacked, same right edge: the taller one, in either order.
    let stacked = [zone(0, 0, 1920, 1080), zone(0, 1080, 1920, 1200)];
    let reversed = [stacked[1], stacked[0]];
    for zones in [stacked, reversed] {
        assert_eq!(
            line(Barrier::on(Edge::Right, &zones).unwrap()),
            (1920, 1080, 1920, 2279)
        );
    }
}

#[test]
fn the_crossing_tells_the_phone_the_edge_and_where_along_it() {
    let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
    let outs = glide.activated(7, (1603.0, 400.4), &keys, Instant::now());
    assert_eq!(
        sent(&outs),
        vec![serde_json::json!({
            "exitEdge": 4, "deltax": 3, "deltay": 400, "edgeLength": 1000
        })]
    );
    assert!(outs.contains(&Out::Gliding(true)));
    assert!(outs.contains(&Out::Clipboard));
    // The top edge is Qt's 1, and its along-the-edge delta is x.
    let mut top = Glide::accepting(Barrier::on(Edge::Top, &screen()).unwrap());
    let outs = top.activated(1, (2000.0, -4.0), &keys, Instant::now());
    assert_eq!(sent(&outs)[0]["exitEdge"], 1);
    assert_eq!(sent(&outs)[0]["deltax"], 1600, "clamped to the edge");
}

#[test]
fn input_before_activation_waits_then_follows_the_crossing() {
    let mut glide = Glide::accepting(Barrier::on(Edge::Left, &screen()).unwrap());
    let now = Instant::now();
    assert!(glide
        .input(Captured::Motion(-5.0, 1.0), &keys, now)
        .is_empty());
    assert!(glide
        .input(Captured::Button(BTN_LEFT, true), &keys, now)
        .is_empty());
    let bodies = sent(&glide.activated(3, (-2.0, 10.0), &keys, now));
    assert_eq!(bodies[0]["exitEdge"], 2);
    assert_eq!(bodies[1], serde_json::json!({"dx": -5.0, "dy": 1.0}));
    assert_eq!(bodies[2], serde_json::json!({"singlehold": true}));
    // Bounded while waiting.
    let mut waiting = Glide::accepting(Barrier::on(Edge::Left, &screen()).unwrap());
    for _ in 0..PENDING_MAX + 10 {
        waiting.input(Captured::Motion(1.0, 0.0), &keys, now);
    }
    assert_eq!(waiting.pending.len(), PENDING_MAX);
    assert_eq!(waiting.dropped, 10);
}

/// Input that waited too long for an Activated (a tail left over after a
/// release) is dropped at the next crossing, not typed into the phone.
#[test]
fn stale_waiting_input_is_not_replayed_at_the_next_crossing() {
    let mut glide = Glide::accepting(Barrier::on(Edge::Left, &screen()).unwrap());
    let then = Instant::now();
    glide.input(Captured::Button(BTN_LEFT, true), &keys, then);
    glide.input(Captured::Key(30, true), &keys, then);
    // A minute later the pointer crosses; two events just before it.
    let now = then + Duration::from_secs(60);
    glide.input(
        Captured::Motion(-3.0, 0.0),
        &keys,
        now - Duration::from_millis(40),
    );
    let bodies = sent(&glide.activated(5, (-1.0, 4.0), &keys, now));
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert_eq!(bodies[0]["exitEdge"], 2);
    assert_eq!(bodies[1], serde_json::json!({"dx": -3.0, "dy": 0.0}));
    assert!(glide.pending.is_empty());
}

#[test]
fn motion_is_coalesced_to_one_packet_per_interval() {
    let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
    let start = Instant::now();
    glide.activated(1, (1600.0, 0.0), &keys, start);
    let first = glide.input(Captured::Motion(1.0, 2.0), &keys, start);
    assert_eq!(
        sent(&first),
        vec![serde_json::json!({"dx": 1.0, "dy": 2.0})]
    );
    assert!(glide
        .input(Captured::Motion(3.0, 0.0), &keys, start)
        .is_empty());
    assert!(glide
        .input(Captured::Motion(1.0, 1.0), &keys, start)
        .is_empty());
    let later = glide.tick(start + MOTION_EVERY);
    assert_eq!(
        sent(&later),
        vec![serde_json::json!({"dx": 4.0, "dy": 1.0})]
    );
    // A click sends the waiting motion first.
    glide.input(Captured::Motion(2.0, 2.0), &keys, start + MOTION_EVERY);
    let click = glide.input(
        Captured::Button(BTN_RIGHT, true),
        &keys,
        start + MOTION_EVERY,
    );
    assert_eq!(
        sent(&click),
        vec![
            serde_json::json!({"dx": 2.0, "dy": 2.0}),
            serde_json::json!({"rightclick": true})
        ]
    );
    // Not finite: ignored.
    glide.input(Captured::Motion(f64::NAN, 1.0), &keys, start);
    assert_eq!(glide.motion, (0.0, 0.0));
}

/// A 1000 Hz gaming mouse for a second: at most one packet per 8 ms
/// (125, was 250 at 4 ms), and the motion they carry adds up exactly.
#[test]
fn a_1000_hz_mouse_sends_one_packet_per_frame_and_loses_no_motion() {
    let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
    let start = Instant::now();
    glide.activated(1, (1600.0, 0.0), &keys, start);
    let (mut packets, mut dx, mut dy) = (0, 0.0, 0.0);
    let mut count = |outs: Vec<Out>| {
        for body in sent(&outs) {
            packets += 1;
            dx += body["dx"].as_f64().unwrap();
            dy += body["dy"].as_f64().unwrap();
        }
    };
    for ms in 0..1000u64 {
        let at = start + Duration::from_millis(ms);
        count(glide.input(Captured::Motion(0.75, -0.25), &keys, at));
    }
    count(glide.tick(start + Duration::from_millis(1000) + MOTION_EVERY));
    assert!(packets <= 126, "{packets} packets");
    assert!(
        (dx - 750.0).abs() < 1e-6 && (dy + 250.0).abs() < 1e-6,
        "{dx},{dy}"
    );
}

#[test]
fn buttons_scroll_and_keys_use_the_kde_shapes() {
    let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
    let now = Instant::now();
    glide.activated(1, (1600.0, 0.0), &keys, now);
    let mut bodies = Vec::new();
    for captured in [
        Captured::Button(BTN_LEFT, false),
        Captured::Button(BTN_RIGHT, false),
        Captured::Button(BTN_MIDDLE, true),
        Captured::Button(BTN_SIDE, true),
        Captured::Discrete(0, 120),
        Captured::Scroll(0.0, 3.5),
        Captured::Key(42, true),
        Captured::Key(30, true),
        Captured::Key(30, false),
        Captured::Key(42, false),
        Captured::Key(29, true),
        Captured::Key(30, true),
        Captured::Key(28, true),
    ] {
        bodies.extend(sent(&glide.input(captured, &keys, now)));
    }
    let no_mods = |mut value: serde_json::Value, shift: bool, ctrl: bool| {
        for (key, on) in [
            ("shift", shift),
            ("ctrl", ctrl),
            ("alt", false),
            ("super", false),
        ] {
            value[key] = on.into();
        }
        value
    };
    assert_eq!(
        bodies,
        vec![
            serde_json::json!({"singlerelease": true}),
            serde_json::json!({"middleclick": true}),
            serde_json::json!({"backclick": true}),
            serde_json::json!({"scroll": true, "dx": 0.0, "dy": -15.0}),
            serde_json::json!({"scroll": true, "dx": 0.0, "dy": 3.5}),
            no_mods(serde_json::json!({"key": "A"}), true, false),
            no_mods(serde_json::json!({"key": "a"}), false, true),
            no_mods(serde_json::json!({"specialKey": 12}), false, true),
        ]
    );
}

#[test]
fn esc_with_super_always_comes_back_where_it_crossed() {
    let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
    let now = Instant::now();
    glide.activated(9, (1600.0, 300.0), &keys, now);
    assert!(sent(&glide.input(Captured::Key(125, true), &keys, now)).is_empty());
    let outs = glide.input(Captured::Key(KEY_ESC, true), &keys, now);
    assert_eq!(sent(&outs), vec![serde_json::json!({"exitEdge": 0})]);
    assert!(outs.contains(&Out::Release {
        activation: 9,
        x: 1597.0,
        y: 300.0
    }));
    assert!(outs.contains(&Out::Gliding(false)));
    assert!(!glide.gliding());
    // Esc alone goes to the phone.
    glide.activated(10, (1600.0, 300.0), &keys, now);
    let esc = glide.input(Captured::Key(KEY_ESC, true), &keys, now);
    assert_eq!(sent(&esc)[0]["specialKey"], 14);
    assert!(glide.gliding());
}

#[test]
fn the_phone_hands_back_at_its_release_point_inside_the_screen() {
    let mut glide = Glide::accepting(Barrier::on(Edge::Left, &screen()).unwrap());
    assert!(
        glide.phone_released(0.0, 10.0, Instant::now()).is_empty(),
        "not gliding"
    );
    glide.activated(4, (0.0, 10.0), &keys, Instant::now());
    let outs = glide.phone_released(0.0, 640.0, Instant::now());
    assert_eq!(
        outs,
        vec![
            Out::Release {
                activation: 4,
                x: 2.0,
                y: 640.0
            },
            Out::Gliding(false)
        ]
    );
    // An old phone's raw pixels past the screen are clamped into it.
    glide.activated(5, (0.0, 10.0), &keys, Instant::now());
    assert_eq!(
        glide.phone_released(0.0, 2400.0, Instant::now())[0],
        Out::Release {
            activation: 5,
            x: 2.0,
            y: 999.0
        }
    );
    // The compositor ending it tells the phone, and releases nothing.
    glide.activated(6, (0.0, 10.0), &keys, Instant::now());
    // Packet ids are millisecond clocks: compare what is sent, not the packets.
    let outs = glide.deactivated();
    assert_eq!(
        sent(&outs),
        vec![serde_json::Value::Object(Glide::taken_back().body)]
    );
    assert!(matches!(
        outs.as_slice(),
        [Out::Send(_), Out::Gliding(false)]
    ));
}

#[test]
fn a_release_needs_both_deltas_finite_and_bounded() {
    let release = |body: serde_json::Value| {
        let mut packet = NetworkPacket::new("desklink.shareinputdevices");
        packet.body = body.as_object().cloned().unwrap_or_default();
        release_deltas(&packet)
    };
    assert_eq!(
        release(serde_json::json!({"releaseDeltax": 0, "releaseDeltay": 640.5})),
        Some((0.0, 640.5))
    );
    assert_eq!(release(serde_json::json!({"releaseDeltay": 3})), None);
    assert_eq!(
        release(serde_json::json!({"releaseDeltax": 0, "releaseDeltay": "3"})),
        None
    );
    assert_eq!(
        release(serde_json::json!({"releaseDeltax": 0, "releaseDeltay": MAX_DELTA + 1.0})),
        None
    );
}

#[test]
fn the_clipboard_follows_only_within_its_bound() {
    assert!(clipboard_packet(String::new()).is_none());
    assert!(clipboard_packet("x".repeat(MAX_CLIPBOARD_BYTES)).is_some());
    assert!(clipboard_packet("x".repeat(MAX_CLIPBOARD_BYTES + 1)).is_none());
}

#[test]
fn a_lost_reader_or_new_screens_release_the_pointer() {
    let live = || {
        let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
        glide.activated(3, (1600.0, 200.0), &keys, Instant::now());
        glide
    };
    for (event, expected) in [
        (Event::ReaderClosed, "Close"),
        (Event::Signal(Signal::ZonesChanged), "Rearm"),
    ] {
        let mut glide = live();
        let (outs, after) = react(event, Some(&mut glide));
        assert!(
            outs.iter()
                .any(|out| matches!(out, Out::Release { activation: 3, .. })),
            "{expected}: the capture is released"
        );
        assert!(outs.contains(&Out::Gliding(false)));
        assert!(format!("{after:?}").starts_with(expected));
        assert!(!glide.gliding());
    }
    // Not gliding: nothing to release, the session still changes.
    let (outs, after) = react(Event::ReaderClosed, None);
    assert!(outs.is_empty() && matches!(after, After::Close(_)));
    let (_, after) = react(Event::Signal(Signal::Disabled), Some(&mut live()));
    assert_eq!(after, After::Rearm);
}

/// A drag in progress when the pointer comes back by any road: the phone
/// is told the button is up before anything else.
#[test]
fn a_held_button_is_let_go_on_every_way_out() {
    let held = || {
        let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
        glide.activated(3, (1600.0, 200.0), &keys, Instant::now());
        glide.input(Captured::Button(BTN_LEFT, true), &keys, Instant::now());
        assert!(glide.left_down);
        glide
    };
    let released = |outs: &[Out]| {
        sent(outs)
            .first()
            .is_some_and(|body| body["singlerelease"] == true)
    };
    assert!(released(&held().take_back()));
    assert!(released(&held().deactivated()));
    assert!(released(&held().phone_released(0.0, 10.0, Instant::now())));
    // Up before the way out: nothing extra.
    let mut glide = held();
    glide.input(Captured::Button(BTN_LEFT, false), &keys, Instant::now());
    assert!(!released(&glide.take_back()));
    // Once only.
    let mut glide = held();
    glide.take_back();
    assert!(glide.let_go().is_none());
}

/// The reader of a session that was closed reports its end a moment
/// later; that must not stop the session opened since.
#[test]
fn a_late_word_from_an_old_reader_is_ignored() {
    assert!(is_current(4, Some(4)));
    assert!(!is_current(3, Some(4)));
    assert!(!is_current(4, None));
}

/// The worker ending, for any reason, gives the pointer back and clears
/// what the top bar shows.
#[test]
fn ending_the_worker_releases_a_capture_that_is_live() {
    let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
    glide.activated(9, (1600.0, 200.0), &keys, Instant::now());
    let mut state = State {
        glide: Some(glide),
        current: Some(GlideSetting {
            device: "e2e-release-all".into(),
            edge: Edge::Right,
        }),
        ..State::new()
    };
    set_status(|status| status.gliding = Some("e2e-release-all".into()));
    state.release_all();
    assert!(state.glide.is_none() && state.portal.is_none());
    assert_eq!(status().gliding, None);
}

/// Before: three failed armings (zones that changed mid-way when a
/// monitor or Extend came and went) turned glide off for the rest of the
/// run. Now the first retry reads the zones again at once, and it backs
/// off to once a minute, never stopping.
#[test]
fn arming_is_retried_at_once_then_backs_off_and_never_gives_up() {
    assert_eq!(arm_retry_after(1), Duration::ZERO, "fresh zones at once");
    assert_eq!(arm_retry_after(2), Duration::from_secs(1));
    assert_eq!(arm_retry_after(3), Duration::from_secs(5));
    let mut last = Duration::ZERO;
    for failures in 1..1000 {
        let after = arm_retry_after(failures);
        assert!(after >= last && after <= MINUTE, "{failures}");
        last = after;
    }
    assert_eq!(last, MINUTE);
}

/// Before: the seventh arming in a minute gave up for the run ("The
/// desktop keeps turning glide off"), and a burst of monitor changes or
/// phone reconnects got there. Now a burst only waits for the minute.
#[test]
fn a_burst_of_armings_waits_for_the_minute_and_is_never_refused() {
    let t = Instant::now();
    let mut armed = VecDeque::new();
    for n in 0..REARMS_PER_MINUTE as u64 {
        let at = t + Duration::from_secs(n);
        assert_eq!(next_arm_at(&armed, at), at, "{n}: at once");
        armed.push_back(at);
    }
    let seventh = t + Duration::from_secs(10);
    assert_eq!(next_arm_at(&armed, seventh), t + MINUTE);
    assert_eq!(next_arm_at(&armed, t + MINUTE), t + MINUTE);
    assert_eq!(
        next_arm_at(&armed, t + 2 * MINUTE),
        t + 2 * MINUTE,
        "later: at once"
    );
}

/// A session GNOME dropped (at a lock) answers GetZones with "denied":
/// before, that was retried three times and then glide stayed off for the
/// run, the dead session kept so even "Ask again" could not help. Now it
/// is dropped and a new one asked for. The bus failing under a portal
/// that restarts is not the session going: before this, every error but
/// a timeout dropped a healthy session.
#[test]
fn only_the_portal_saying_the_session_is_gone_drops_it() {
    let gone = [
        "Desktop permission request GetZones was denied",
        "Desktop permission request GetZones was cancelled",
        "Desktop portal GetZones failed: GDBus.Error:org.freedesktop.DBus.Error.AccessDenied: \
             Invalid session",
        "Desktop portal GetZones failed: GDBus.Error:org.freedesktop.DBus.Error.UnknownMethod: \
             Object does not exist at path \u{201c}/org/gnome/Mutter/InputCapture/Session/u2\u{201d}",
    ];
    for error in gone {
        assert!(
            matches!(zones_error(error.into()), ArmError::Gone(_)),
            "{error}"
        );
    }
    let bus = [
        "Timed out waiting for desktop portal GetZones",
        "Desktop portal GetZones failed: GDBus.Error:org.freedesktop.DBus.Error.ServiceUnknown: \
             The name is not activatable",
        "Desktop portal GetZones failed: GDBus.Error:org.freedesktop.DBus.Error.NoReply: \
             Message recipient disconnected from message bus without replying",
        "Desktop portal GetZones failed: Operation was cancelled",
        "Desktop portal GetZones returned no request handle",
    ];
    for error in bus {
        assert!(
            matches!(zones_error(error.into()), ArmError::Retry(_)),
            "{error}"
        );
    }
}

/// One "gone" answer gets one more fresh GetZones at once; the second
/// drops the session.
#[test]
fn a_session_is_dropped_only_after_a_second_gone_answer() {
    let mut state = State::new();
    state.retry_at = Some(Instant::now() + MINUTE);
    assert!(!state.session_gone(), "first: try again");
    assert_eq!(state.retry_at, None, "at once");
    assert!(state.session_gone(), "second: drop it");
}

/// Before: a forward step of the wall clock (NTP after boot) read as a
/// sleep and asked at once. Now only time the computer spent suspended
/// (boot clock minus monotonic clock) counts.
#[test]
fn a_sleep_is_the_suspended_time_growing_not_the_wall_clock() {
    let secs = Duration::from_secs;
    assert!(!slept(secs(0), secs(0)));
    assert!(!slept(secs(100), secs(103)), "rounding");
    assert!(slept(secs(100), secs(3700)), "an hour suspended");
    let (before, now) = (suspended_so_far(), suspended_so_far());
    assert!(!slept(before, now), "no suspend during a test");
}

/// Before: every Disabled or ZonesChanged re-armed at once, six times a
/// minute for ever if the desktop kept disabling it. Now the first two
/// re-arm at once and a storm backs off; and nothing keeps acting on the
/// barrier that was disabled.
#[test]
fn a_storm_of_disables_backs_off_and_drops_the_old_barrier() {
    let t = Instant::now();
    let mut state = State::new();
    state.glide = Some(Glide::accepting(
        Barrier::on(Edge::Right, &screen()).unwrap(),
    ));
    state.disabled_by_desktop(t);
    assert!(state.glide.is_none(), "the old barrier is gone");
    assert_eq!(state.retry_at, None, "first: at once");
    state.disabled_by_desktop(t + Duration::from_secs(1));
    assert_eq!(state.retry_at, None, "a monitor change sends two");
    let mut last = Duration::ZERO;
    for n in 2..20u64 {
        let at = t + Duration::from_secs(n);
        state.disabled_by_desktop(at);
        let wait = state.retry_at.map(|retry| retry - at).unwrap_or_default();
        assert!(wait >= last && wait <= MINUTE, "{n}: {wait:?}");
        last = wait;
    }
    assert_eq!(last, MINUTE);
    assert!(state.disables.len() <= 16);
    // A quiet minute later: at once again.
    state.disabled_by_desktop(t + 3 * MINUTE);
    assert_eq!(state.retry_at, None);
    assert_eq!(rearm_delay(3), Duration::from_secs(1));
}

/// The keymap's answers are remembered for a while and then asked again;
/// an answer of "no such key" is one, but a lookup that timed out never
/// reaches the cache (it returns before `put`).
#[test]
fn the_key_cache_expires_and_is_bounded() {
    let mut cache = KeyCache::default();
    let now = Instant::now();
    let key = (30, Mods::default());
    assert_eq!(cache.get(key, now), None, "never asked");
    cache.put(key, None, now);
    assert_eq!(cache.get(key, now), Some(None), "no such key, answered");
    cache.put((31, Mods::default()), Some(97), now);
    assert_eq!(cache.get((31, Mods::default()), now), Some(Some(97)));
    assert_eq!(
        cache.get(key, now + KEY_TTL + Duration::from_secs(1)),
        None,
        "a layout switch is noticed"
    );
    for code in 0..(KEY_CACHE_MAX as u32 + 10) {
        cache.put((code + 100, Mods::default()), Some(1), now);
    }
    assert!(cache.0.len() <= KEY_CACHE_MAX);
}

/// A link that went quiet under the pointer gives it back; a quiet phone
/// the pointer is not on is simply idle.
#[test]
fn a_quiet_link_under_the_pointer_gives_it_back() {
    let quiet = Some(GLIDE_SILENT_AFTER + Duration::from_millis(1));
    assert!(link_silent(true, quiet, false));
    assert!(!link_silent(false, quiet, false), "idle, not gone");
    assert!(
        !link_silent(true, quiet, true),
        "a dozing phone is not handed back"
    );
    assert!(!link_silent(true, Some(Duration::from_secs(5)), false));
    assert!(!link_silent(true, None, false));
    // Longer than the liveness probe, shorter than the 15 s rebuild.
    assert!(GLIDE_SILENT_AFTER > crate::device_links::webrtc::coordinator::PEER_PROBE_AFTER);
    assert!(GLIDE_SILENT_AFTER < crate::device_links::webrtc::coordinator::PEER_DEAD_AFTER);
}

/// A simulated minute, a tick a quarter second, with the two clocks the
/// way a real phone drives them: features and heartbeats on the data
/// channel, and only the reply to the desk's 10 s probe on the LAN (a
/// sawtooth of 0 to 10.x s). `data_every` is how often the data channel
/// speaks (None: never). Whether the pointer was ever handed back.
fn handed_back_within(data_every: Option<u64>, dozing: bool) -> Option<u64> {
    use crate::device_links::webrtc::coordinator::link_silence;
    const TICK_MS: u64 = 250;
    let (mut lan_ms, mut data_ms) = (0u64, 0u64);
    for tick in 1..=(60_000 / TICK_MS) {
        let now = tick * TICK_MS;
        lan_ms += TICK_MS;
        data_ms += TICK_MS;
        // The LAN probe goes out after 10 s of LAN silence and is
        // answered a moment later.
        if lan_ms > 10_500 {
            lan_ms = 0;
        }
        if data_every.is_some_and(|every| now % (every * 1000) == 0) {
            data_ms = 0;
        }
        let silence = link_silence(
            Some(Duration::from_millis(lan_ms)),
            data_every.map(|_| Duration::from_millis(data_ms)),
        );
        if link_silent(true, silence, dozing) {
            return Some(now / 1000);
        }
    }
    None
}

#[test]
fn a_healthy_phone_is_never_handed_back_and_a_silent_data_channel_is() {
    // Talks every 2 s: not once in a minute (the LAN sawtooth alone would).
    assert_eq!(handed_back_within(Some(2), false), None);
    // Talks every 5 s (the liveness probe's pace): still never.
    assert_eq!(handed_back_within(Some(5), false), None);
    // The LAN clock alone, as it was: the bug (some second under 20).
    let lan_alone = {
        let mut handed = None;
        let mut lan_ms = 0u64;
        for tick in 1..=240u64 {
            lan_ms += 250;
            if lan_ms > 10_500 {
                lan_ms = 0;
            }
            if link_silent(true, Some(Duration::from_millis(lan_ms)), false) {
                handed = Some(tick / 4);
                break;
            }
        }
        handed
    };
    assert!(lan_alone.is_some_and(|second| second < 20), "{lan_alone:?}");
    // A phone whose data channel never speaks is handed back at 8 s or
    // when the LAN probe's sawtooth next passes it (it never does here: it
    // resets before 11 s, so it is the sawtooth's last stretch).
    assert!(handed_back_within(None, false).is_some_and(|second| second <= 12));
    // Dozing: never, whatever the clocks say.
    assert_eq!(handed_back_within(None, true), None);
}

#[test]
fn the_two_clocks_say_the_shorter_silence() {
    use crate::device_links::webrtc::coordinator::link_silence;
    let s = Duration::from_secs;
    assert_eq!(link_silence(Some(s(9)), Some(s(1))), Some(s(1)));
    assert_eq!(link_silence(Some(s(1)), Some(s(9))), Some(s(1)));
    assert_eq!(link_silence(Some(s(4)), None), Some(s(4)));
    assert_eq!(link_silence(None, Some(s(4))), Some(s(4)));
    assert_eq!(link_silence(None, None), None);
}

/// Nothing wakes the worker more than once a second while glide is off.
#[test]
fn the_worker_sleeps_long_when_glide_is_off() {
    let mut state = State::new();
    assert_eq!(nap(&state), OFF_NAP);
    const { assert!(OFF_NAP.as_millis() >= 1000) };
    let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
    state.glide = Some(Glide::accepting(
        Barrier::on(Edge::Right, &screen()).unwrap(),
    ));
    assert_eq!(
        nap(&state),
        CHECK_EVERY,
        "armed without a portal is still off"
    );
    glide.activated(1, (1600.0, 1.0), &keys, Instant::now());
    state.glide = Some(glide);
    assert_eq!(nap(&state), MOTION_EVERY);
}

/// The ask the phone reads (its `Glide.asksToTurnOn`): exactly
/// `{set: {glide: true}}`, and only to a phone whose build takes it.
#[test]
fn the_ask_to_turn_glide_on_is_one_allow_listed_key() {
    let ask = turn_on_request();
    assert_eq!(ask.packet_type, PACKET_TYPE_SETTINGS_REQUEST);
    assert_eq!(
        serde_json::Value::Object(ask.body.clone()),
        serde_json::json!({"set": {"glide": true}})
    );
    assert!(can_ask_phone(&[PACKET_TYPE_SETTINGS_REQUEST.to_string()]));
    assert!(
        !can_ask_phone(&["desklink.settings".to_string()]),
        "an older phone"
    );
}

#[test]
fn the_setting_reads_and_writes_plain_edge_names() {
    let setting = GlideSetting {
        device: "p".into(),
        edge: Edge::Bottom,
    };
    let json = serde_json::to_string(&setting).unwrap();
    assert_eq!(json, r#"{"device":"p","edge":"bottom"}"#);
    assert_eq!(
        serde_json::from_str::<GlideSetting>(&json).unwrap(),
        setting
    );
}

// ── W6: the phone must allow glide, and accept each entry ───────────────

fn left() -> Glide {
    Glide::new(Barrier::on(Edge::Left, &screen()).unwrap())
}

fn count_refused(outs: &[Out]) -> usize {
    outs.iter().filter(|out| **out == Out::Refused).count()
}

fn released_at(outs: &[Out]) -> Option<(f64, f64)> {
    outs.iter().find_map(|out| match out {
        Out::Release { x, y, .. } => Some((*x, *y)),
        _ => None,
    })
}

/// A phone that hands the pointer back: it is released where it crossed,
/// nothing but the entry was streamed, this edge is left alone for 5 s
/// and the person is told once.
#[test]
fn a_phone_that_hands_the_pointer_back_gets_nothing_and_the_edge_rests() {
    let mut glide = left();
    let start = Instant::now();
    let mut streamed = sent(&glide.activated(1, (-2.0, 300.0), &keys, start)).len();
    assert_eq!(streamed, 1, "only the entry itself");
    // The mouse keeps moving and clicking while the phone has not answered.
    for step in 0..50 {
        let at = start + Duration::from_millis(step);
        streamed += sent(&glide.input(Captured::Motion(-3.0, 1.0), &keys, at)).len();
        streamed += sent(&glide.input(Captured::Button(BTN_LEFT, true), &keys, at)).len();
    }
    assert_eq!(streamed, 1, "no event streams before an accept");
    // The phone refuses 20 ms in.
    let at = start + Duration::from_millis(20);
    let outs = glide.phone_released(0.0, 0.0, at);
    let (x, y) = released_at(&outs).expect("the pointer is given back");
    assert!(
        (0.0..=5.0).contains(&x) && (y - 300.0).abs() < 2.0,
        "{x},{y}: at the crossing"
    );
    assert!(
        sent(&outs).is_empty(),
        "nothing is said to a phone that said no"
    );
    assert!(outs.contains(&Out::Gliding(false)));
    assert_eq!(count_refused(&outs), 1, "one toast");
    assert!(!glide.gliding());
    // Pushing on the edge again within 5 s: released at once, silent.
    let again = glide.activated(2, (-1.0, 310.0), &keys, start + Duration::from_secs(4));
    assert!(sent(&again).is_empty() && released_at(&again).is_some());
    assert!(!glide.gliding(), "never captured");
    // After 5 s it tries again; a second refusal is not announced again.
    let later = start + Duration::from_secs(6);
    assert_eq!(
        sent(&glide.activated(3, (-2.0, 300.0), &keys, later)).len(),
        1
    );
    let outs = glide.phone_released(0.0, 0.0, later + Duration::from_millis(10));
    assert_eq!(count_refused(&outs), 0, "told once");
    // An accept ends the silence: the next refusal is announced.
    let t = later + Duration::from_secs(6);
    glide.activated(4, (-2.0, 300.0), &keys, t);
    glide.accepted(&keys, t);
    glide.take_back();
    let t = t + Duration::from_secs(6);
    glide.activated(5, (-2.0, 300.0), &keys, t);
    let outs = glide.phone_released(0.0, 0.0, t);
    assert_eq!(count_refused(&outs), 1);
}

/// No answer within a second is a refusal too: the pointer is back where
/// it crossed, the phone is told to forget the entry, and the edge rests
/// only a second (the phone said no to nothing). A phone that answers in
/// 400 ms (a dozed Wi-Fi, a busy event thread) is not refused: at 300 ms
/// it was, and the edge then rested 5 s.
#[test]
fn an_entry_the_phone_never_answers_is_given_back_after_a_second() {
    let mut slow = left();
    let start = Instant::now();
    slow.activated(1, (-2.0, 120.0), &keys, start);
    assert!(slow.tick(start + Duration::from_millis(400)).is_empty());
    let outs = slow.accepted(&keys, start + Duration::from_millis(400));
    assert_eq!(count_refused(&outs), 0);
    assert!(slow.gliding(), "a slow accept still glides");

    let mut glide = left();
    glide.activated(1, (-2.0, 120.0), &keys, start);
    glide.input(Captured::Motion(-9.0, 0.0), &keys, start);
    assert!(glide.tick(start + Duration::from_millis(999)).is_empty());
    assert!(glide.gliding());
    let outs = glide.tick(start + ENTER_WAIT);
    assert_eq!(sent(&outs), vec![serde_json::json!({"exitEdge": 0})]);
    assert!(released_at(&outs).is_some());
    assert_eq!(count_refused(&outs), 1);
    assert!(!glide.gliding());
    assert!(glide.tick(start + Duration::from_secs(2)).is_empty());
    // The next push on the edge after a second tries again.
    let again = start + ENTER_WAIT + UNANSWERED_BACKOFF;
    assert_eq!(
        sent(&glide.activated(2, (-2.0, 120.0), &keys, again)).len(),
        1
    );
    assert!(glide.gliding());
}

/// An ask waits for the person on the phone: Allow ends it, "Not now"
/// says so, and nobody answering for a minute says that.
#[test]
fn an_ask_waits_for_the_phone_and_ends_clearly() {
    assert_eq!(
        ask_words("Pixel", PhoneAsk::Waiting),
        "Waiting for Pixel: tap Allow there"
    );
    assert_eq!(
        ask_words("Pixel", PhoneAsk::TimedOut),
        "Pixel didn't answer"
    );

    // Through the phone's reports.
    let phone = &format!(
        "{}ask-phone-state",
        crate::device_links::desk_features::TEST_OUTBOX_PREFIX
    );
    let report = |body: serde_json::Value| {
        let mut packet =
            NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_SHARE_INPUT_DEVICES);
        for (key, value) in body.as_object().unwrap() {
            packet.body.insert(key.clone(), value.clone());
        }
        packet
    };
    on_phone_packet(phone, &report(serde_json::json!({"allowed": false})));
    crate::device_links::asks::ask_phone(phone, PHONE_SETTING_KEY, true);
    assert_eq!(phone_ask(phone), Some(PhoneAsk::Waiting));
    on_phone_packet(
        phone,
        &report(serde_json::json!({"allowed": false, "reason": "locked"})),
    );
    assert_eq!(
        phone_ask(phone),
        Some(PhoneAsk::Waiting),
        "locked is not an answer"
    );
    on_phone_packet(
        phone,
        &report(serde_json::json!({"allowed": false, "reason": "declined"})),
    );
    assert_eq!(phone_ask(phone), Some(PhoneAsk::Declined));
    crate::device_links::asks::ask_phone(phone, PHONE_SETTING_KEY, true);
    on_phone_packet(phone, &report(serde_json::json!({"allowed": true})));
    assert_eq!(phone_ask(phone), None, "Allow ends it");
    on_phone_packet(phone, &report(serde_json::json!({"allowed": false})));
    assert_eq!(phone_ask(phone), None, "and it is not shown again later");
    forget_phone(phone);
    crate::device_links::asks::forget(phone);
}

/// Setting Glide to a phone that has it off asks the phone once; a phone
/// that allows it, or is only locked, is not asked; no report yet waits.
#[test]
fn setting_glide_to_a_phone_that_has_it_off_asks_there_once() {
    assert_eq!(auto_ask(true, Some(false), false), AutoAsk::Now);
    assert_eq!(auto_ask(true, None, false), AutoAsk::Later);
    assert_eq!(auto_ask(true, Some(true), false), AutoAsk::Never);
    assert_eq!(
        auto_ask(true, Some(false), true),
        AutoAsk::Never,
        "locked: unlock, not allow"
    );
    assert_eq!(
        auto_ask(false, Some(false), false),
        AutoAsk::Never,
        "only when the person just set it"
    );
}

/// Accepted: what was held goes out in order, then it streams normally.
#[test]
fn an_accepted_entry_replays_what_was_held_and_then_streams() {
    let mut glide = left();
    let start = Instant::now();
    glide.activated(1, (-2.0, 120.0), &keys, start);
    glide.input(Captured::Motion(-4.0, 2.0), &keys, start);
    glide.input(Captured::Button(BTN_LEFT, true), &keys, start);
    glide.input(Captured::Button(BTN_LEFT, false), &keys, start);
    let at = start + Duration::from_millis(40);
    let outs = glide.accepted(&keys, at);
    assert_eq!(
        sent(&outs),
        vec![
            serde_json::json!({"dx": -4.0, "dy": 2.0}),
            serde_json::json!({"singlehold": true}),
            serde_json::json!({"singlerelease": true}),
        ]
    );
    // After the accept: streaming at once, and no timeout any more.
    assert!(glide.tick(start + Duration::from_secs(2)).is_empty());
    assert!(glide.gliding());
    let outs = glide.input(
        Captured::Motion(1.0, 1.0),
        &keys,
        at + Duration::from_millis(10),
    );
    assert_eq!(sent(&outs).len(), 1);
    // A second accept changes nothing.
    assert!(glide.accepted(&keys, at).is_empty());
}

/// Held input is bounded, and Esc+Super still brings the pointer back
/// while the phone has not answered.
#[test]
fn held_input_is_bounded_and_esc_super_works_while_waiting() {
    let mut glide = left();
    let now = Instant::now();
    glide.activated(1, (-2.0, 120.0), &keys, now);
    for _ in 0..HELD_MAX + 20 {
        glide.input(Captured::Motion(1.0, 0.0), &keys, now);
    }
    assert_eq!(glide.held.len(), HELD_MAX);
    glide.input(Captured::Key(125, true), &keys, now);
    let outs = glide.input(Captured::Key(KEY_ESC, true), &keys, now);
    assert!(released_at(&outs).is_some(), "Esc+Super comes back");
    assert!(!glide.gliding());
    assert!(glide.held.is_empty());
}

/// The phone says glide is no longer allowed: a pointer on it, or on its
/// way, comes back at once.
#[test]
fn a_flip_to_not_allowed_gives_the_pointer_back_at_once() {
    let now = Instant::now();
    let mut on = left();
    on.activated(1, (-2.0, 200.0), &keys, now);
    on.accepted(&keys, now);
    on.input(Captured::Button(BTN_LEFT, true), &keys, now);
    let outs = on.disallowed(now);
    assert_eq!(
        sent(&outs),
        vec![
            serde_json::json!({"singlerelease": true}),
            serde_json::json!({"exitEdge": 0})
        ]
    );
    assert!(released_at(&outs).is_some() && !on.gliding());
    // Still entering: a refusal, nothing said.
    let mut entering = left();
    entering.activated(1, (-2.0, 200.0), &keys, now);
    let outs = entering.disallowed(now);
    assert!(released_at(&outs).is_some() && sent(&outs).is_empty());
}

/// The barrier is armed only for a phone that reported "allowed: true";
/// unknown (an older phone) and false both leave it disabled.
#[test]
fn the_barrier_is_armed_only_when_the_phone_allows_glide() {
    assert!(wants_barrier(false, true, true));
    assert!(!wants_barrier(false, true, false), "not allowed or unknown");
    assert!(!wants_barrier(false, false, true), "link not ready");
    assert!(!wants_barrier(true, true, true), "link silent");
    let phone = "w6f-phone-state";
    assert_eq!(phone_allows(phone), None, "no report yet");
    assert!(!allows_glide(phone));
    let report = |allowed: bool| {
        let mut packet =
            NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_SHARE_INPUT_DEVICES);
        packet.set("allowed", allowed);
        packet
    };
    on_phone_packet(phone, &report(false));
    assert_eq!(phone_allows(phone), Some(false));
    assert!(!allows_glide(phone));
    on_phone_packet(phone, &report(true));
    assert!(allows_glide(phone));
    on_phone_packet(phone, &report(false));
    assert!(!allows_glide(phone), "a flip to false is seen at once");
    // A release (G3) is not a state report.
    let mut release =
        NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_SHARE_INPUT_DEVICES);
    release.set("releaseDeltax", 3);
    release.set("releaseDeltay", 4);
    on_phone_packet(phone, &release);
    assert_eq!(phone_allows(phone), Some(false));
    set_allowed(phone, None);
    assert_eq!(phone_allows(phone), None, "forgotten when the link goes");
}

#[test]
fn a_phone_with_its_accessibility_service_off_is_not_asked_to_allow_glide() {
    let report = |body: serde_json::Value| {
        let mut packet =
            NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_SHARE_INPUT_DEVICES);
        for (key, value) in body.as_object().unwrap() {
            packet.body.insert(key.clone(), value.clone());
        }
        packet
    };
    let phone = "p39-service";
    on_phone_packet(
        phone,
        &report(serde_json::json!({"allowed": false, "reason": "accessibility"})),
    );
    assert_eq!(phone_allows(phone), Some(false));
    assert!(phone_needs_service(phone));
    assert!(!phone_locked(phone));
    // Nothing for the auto-ask to do: allowing Glide there changes nothing.
    assert_eq!(
        auto_ask(true, Some(false), phone_needs_service(phone)),
        AutoAsk::Never
    );
    // The next report that allows it clears the step.
    on_phone_packet(phone, &report(serde_json::json!({"allowed": true})));
    assert!(!phone_needs_service(phone));
    forget_phone(phone);
}

// ---- Fault-injection simulation -------------------------------------------
// Random event sequences (fixed seed) against the state machine with a model of
// the portal and of the phone's held button. Every sequence may contain lost
// input, link drops (take_back), double enters, an enter during a hand-back,
// the phone saying no, the compositor ending the capture, and silence past
// ENTER_WAIT. Invariants: no button is left held on the phone once the
// pointer is back, the portal capture is released with it, the state
// machine and the model agree on where the pointer is, an unanswered enter
// ends within ENTER_WAIT, and every sequence ends in a known idle.

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// What the portal and the phone would hold, from the outs alone.
#[derive(Default)]
struct World {
    captured: Option<u32>,
    left_held: bool,
    on_phone: bool,
    refused: u32,
}

impl World {
    fn apply(&mut self, outs: Vec<Out>) {
        for out in outs {
            match out {
                Out::Send(packet) => {
                    if packet.body.get("singlehold").is_some() {
                        self.left_held = true;
                    }
                    if packet.body.get("singlerelease").is_some() {
                        self.left_held = false;
                    }
                }
                Out::Release { activation, .. } => {
                    if self.captured == Some(activation) {
                        self.captured = None;
                    }
                }
                Out::Gliding(on) => self.on_phone = on,
                Out::Refused => self.refused += 1,
                Out::Clipboard => {}
            }
        }
    }
}

fn check(glide: &Glide, world: &World, seed: u64, step: usize) {
    let tag = format!("seed {seed} step {step}");
    // Pointer here -> nothing held on the phone, nothing captured by us.
    if glide.active.is_none() && world.captured.is_some() {
        // Only a capture the compositor itself still holds (an Activated that
        // came after our release) may remain; then Gliding must not be on.
        assert!(!world.on_phone, "{tag}: on phone without an active glide");
    }
    if glide.active.is_none() {
        assert!(!glide.left_down, "{tag}: left_down while idle");
        assert!(!world.left_held, "{tag}: phone left holding a button");
    }
    assert_eq!(
        world.on_phone,
        glide.active.is_some(),
        "{tag}: indicator disagrees with the machine"
    );
}

#[test]
fn simulated_faults_never_leave_the_pointer_or_a_button_stuck() {
    let barrier = Barrier::on(Edge::Right, &screen()).unwrap();
    for seed in 1..=3000u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut glide = if rng.below(4) == 0 {
            Glide::accepting(barrier)
        } else {
            Glide::new(barrier)
        };
        let mut world = World::default();
        let base = Instant::now();
        let mut at = Duration::ZERO;
        let mut activation = 0u32;
        let mut entering_since: Option<Duration> = None;
        for step in 0..80 {
            at += Duration::from_millis(rng.below(400));
            let now = base + at;
            let outs = match rng.below(16) {
                0 | 1 => {
                    // The portal activates (sometimes again while already active).
                    activation += 1;
                    world.captured = Some(activation);
                    entering_since.get_or_insert(at);
                    glide.activated(activation, (1600.0, rng.below(1000) as f64), &keys, now)
                }
                2 => {
                    world.captured = None; // the compositor ended it
                    entering_since = None;
                    glide.deactivated()
                }
                3 => glide.take_back(), // link down, glide off, zones changed, lock
                4 => glide.phone_released(rng.below(50) as f64, rng.below(900) as f64, now),
                5 => {
                    entering_since = None;
                    glide.accepted(&keys, now)
                }
                6 => {
                    entering_since = None;
                    glide.disallowed(now)
                }
                7 => glide.tick(now + Duration::from_secs(rng.below(3))),
                8 => glide.input(Captured::Button(BTN_LEFT, true), &keys, now),
                9 => glide.input(Captured::Button(BTN_LEFT, false), &keys, now),
                10 => glide.input(Captured::Button(BTN_RIGHT, rng.below(2) == 0), &keys, now),
                11 => glide.input(
                    Captured::Motion(rng.below(40) as f64 - 20.0, 3.0),
                    &keys,
                    now,
                ),
                12 => glide.input(Captured::Scroll(0.0, 4.0), &keys, now),
                13 => glide.input(Captured::Key(30, rng.below(2) == 0), &keys, now),
                14 => {
                    // Lost input: nothing arrives; time passes.
                    at += Duration::from_millis(900);
                    Vec::new()
                }
                _ => glide.input(Captured::Closed, &keys, now),
            };
            world.apply(outs);
            if glide.active.is_none() {
                entering_since = None;
            }
            // An unanswered enter ends within ENTER_WAIT of its first tick.
            if let (Some(since), true) = (entering_since, glide.active.is_some()) {
                if at.saturating_sub(since) > ENTER_WAIT + Duration::from_secs(3) {
                    let tick = glide.tick(base + at);
                    world.apply(tick);
                }
            }
            check(&glide, &world, seed, step);
        }
        // Whatever happened, taking the pointer back ends in a known idle.
        world.apply(glide.take_back());
        world.apply(glide.deactivated());
        assert!(glide.active.is_none(), "seed {seed}: not idle");
        assert!(!world.left_held, "seed {seed}: button held at the end");
        assert!(!world.on_phone, "seed {seed}: indicator on at the end");
        assert!(world.captured.is_none() || glide.active.is_none());
    }
}

#[test]
fn an_unanswered_enter_is_refused_within_the_wait_and_told_once() {
    let barrier = Barrier::on(Edge::Right, &screen()).unwrap();
    let mut glide = Glide::new(barrier);
    let now = Instant::now();
    glide.activated(1, (1600.0, 300.0), &keys, now);
    assert!(glide.tick(now + ENTER_WAIT / 2).is_empty());
    let outs = glide.tick(now + ENTER_WAIT);
    assert!(outs.iter().any(|o| matches!(o, Out::Release { .. })));
    assert!(outs.iter().any(|o| matches!(o, Out::Refused)));
    assert!(glide.active.is_none());
    // The edge rests, then a second silent crossing is released without a second notice.
    let again = glide.activated(
        2,
        (1600.0, 300.0),
        &keys,
        now + ENTER_WAIT + Duration::from_millis(10),
    );
    assert!(again.iter().any(|o| matches!(o, Out::Release { .. })));
    assert!(!again.iter().any(|o| matches!(o, Out::Refused)));
}

// ---- Deterministic fault cases ---------------------------------------------

fn count(outs: &[Out], what: impl Fn(&Out) -> bool) -> usize {
    outs.iter().filter(|out| what(out)).count()
}
fn is_release(out: &Out) -> bool {
    matches!(out, Out::Release { .. })
}
fn is_off(out: &Out) -> bool {
    matches!(out, Out::Gliding(false))
}
fn right() -> Glide {
    Glide::new(Barrier::on(Edge::Right, &screen()).unwrap())
}

#[test]
fn reports_and_ticks_while_idle_do_nothing() {
    let mut glide = right();
    let now = Instant::now();
    assert!(glide.phone_released(1.0, 2.0, now).is_empty());
    assert!(glide.accepted(&keys, now).is_empty());
    assert!(glide.disallowed(now).is_empty());
    assert!(glide.tick(now).is_empty());
    assert!(glide.take_back().is_empty());
    assert!(glide.deactivated().is_empty());
    assert!(glide.active.is_none());
}

#[test]
fn a_lost_release_is_let_go_first_when_the_pointer_comes_back() {
    let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
    let now = Instant::now();
    glide.activated(1, (1600.0, 10.0), &keys, now);
    glide.input(Captured::Button(BTN_LEFT, true), &keys, now);
    assert!(glide.left_down);
    // The release never arrives (lost): the way out sends it.
    let outs = glide.take_back();
    assert!(matches!(&outs[0], Out::Send(p) if p.body.contains_key("singlerelease")));
    assert!(!glide.left_down);
}

#[test]
fn a_crossing_during_the_resting_edge_is_released_at_once_and_works_after_it() {
    let mut glide = right();
    let now = Instant::now();
    glide.activated(1, (1600.0, 10.0), &keys, now);
    let refusal = glide.phone_released(0.0, 0.0, now);
    assert_eq!(count(&refusal, |o| matches!(o, Out::Refused)), 1);
    let rest = now + Duration::from_secs(1);
    let outs = glide.activated(2, (1600.0, 10.0), &keys, rest);
    assert_eq!(count(&outs, is_release), 1);
    assert_eq!(count(&outs, |o| matches!(o, Out::Refused)), 0, "told once");
    assert!(glide.active.is_none());
    let outs = glide.activated(
        3,
        (1600.0, 10.0),
        &keys,
        now + REFUSED_BACKOFF + Duration::from_millis(1),
    );
    assert!(glide.active == Some(3) && count(&outs, is_release) == 0);
}

#[test]
fn a_second_activation_while_entering_leaves_one_capture_to_release() {
    let mut glide = right();
    let now = Instant::now();
    glide.activated(1, (1600.0, 10.0), &keys, now);
    // The compositor activated again (the first capture ended by itself).
    glide.activated(2, (1600.0, 20.0), &keys, now);
    let outs = glide.tick(now + ENTER_WAIT);
    assert_eq!(count(&outs, is_release), 1);
    assert!(matches!(
        outs.iter().find(|o| is_release(o)),
        Some(Out::Release { activation: 2, .. })
    ));
}

#[test]
fn an_accept_after_the_compositor_ended_the_capture_resurrects_nothing() {
    let mut glide = right();
    let now = Instant::now();
    glide.activated(1, (1600.0, 10.0), &keys, now);
    glide.deactivated();
    assert!(glide.accepted(&keys, now).is_empty());
    assert!(glide.active.is_none());
}

#[test]
fn a_link_drop_in_every_state_ends_idle_with_one_release_and_one_indicator_off() {
    let now = Instant::now();
    type Setup = (&'static str, Box<dyn Fn(&mut Glide)>, bool);
    let setups: Vec<Setup> = vec![
        ("idle", Box::new(|_| {}), false),
        (
            "entering",
            Box::new(move |g| {
                g.activated(1, (1600.0, 5.0), &keys, now);
            }),
            true,
        ),
        (
            "active",
            Box::new(move |g| {
                g.activated(1, (1600.0, 5.0), &keys, now);
                g.accepted(&keys, now);
            }),
            true,
        ),
        (
            "left held",
            Box::new(move |g| {
                g.activated(1, (1600.0, 5.0), &keys, now);
                g.accepted(&keys, now);
                g.input(Captured::Button(BTN_LEFT, true), &keys, now);
            }),
            true,
        ),
        (
            "pending",
            Box::new(move |g| {
                g.input(Captured::Motion(1.0, 1.0), &keys, now);
            }),
            false,
        ),
        (
            "held",
            Box::new(move |g| {
                g.activated(1, (1600.0, 5.0), &keys, now);
                g.input(Captured::Key(30, true), &keys, now);
            }),
            true,
        ),
    ];
    for (name, setup, was_active) in setups {
        let mut glide = right();
        setup(&mut glide);
        let outs = glide.take_back();
        assert!(glide.active.is_none(), "{name}");
        assert!(
            !glide.left_down && glide.held.is_empty() && glide.pending.is_empty(),
            "{name}"
        );
        assert_eq!(count(&outs, is_release), usize::from(was_active), "{name}");
        assert_eq!(count(&outs, is_off), usize::from(was_active), "{name}");
        assert!(
            glide.take_back().is_empty(),
            "{name}: a second drop does nothing"
        );
    }
}

#[test]
fn input_held_past_the_limit_is_never_replayed_after_a_refusal() {
    let mut glide = right();
    let now = Instant::now();
    glide.activated(1, (1600.0, 10.0), &keys, now);
    for _ in 0..HELD_MAX + 30 {
        glide.input(Captured::Key(30, true), &keys, now);
    }
    assert!(glide.dropped >= 30);
    let outs = glide.disallowed(now);
    assert!(sent(&outs).is_empty() || sent(&outs).iter().all(|p| p.get("exitEdge").is_some()));
    assert!(glide.held.is_empty());
    assert!(glide.accepted(&keys, now).is_empty());
}

#[test]
fn a_phone_that_goes_silent_gets_the_pointer_back_unless_it_is_dozing() {
    let late = GLIDE_SILENT_AFTER + Duration::from_millis(1);
    assert!(link_silent(true, Some(late), false));
    assert!(
        !link_silent(true, Some(late), true),
        "a dozing phone is kept by the LAN heartbeat"
    );
    assert!(!link_silent(true, Some(GLIDE_SILENT_AFTER), false));
    assert!(
        !link_silent(false, Some(late), false),
        "nothing to give back"
    );
    assert!(!link_silent(true, None, false));
}
