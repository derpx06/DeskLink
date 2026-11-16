//! Tests of the terminal bridge: consent and session-password gates, the
//! outbox order and budget, attach/detach and the size claim, snapshots,
//! ending sessions and the idle reaper, against a real PTY registry.

use super::*;
use crate::device_links::config::Config;
use crate::device_links::core::SessionLink;
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::webrtc::HandoverRuntime;
use std::time::{Duration, Instant};
use uuid::Uuid;

fn wire(peer: &str) -> WebRtcWireBinding {
    WebRtcWireBinding {
        sender_device_id: "desktop-1".to_string(),
        peer_device_id: peer.to_string(),
        session_id: 7,
        generation: 3,
    }
}

fn new_bridge(peer: &str) -> TerminalBridge {
    TerminalBridge::new(wire(peer), NEXT_TOKEN.fetch_add(1, Ordering::Relaxed))
}

fn request(action: TerminalAction, wire: &WebRtcWireBinding) -> TerminalControl {
    let mut control = TerminalControl::new(action, wire);
    control.device_id = wire.peer_device_id.clone();
    control
}

/// Waits for a line of input, prints `text`, then stays alive.
///
/// Two things here are load-bearing:
///
/// * The sleep: a program that exits immediately can be reaped by the
///   `Sessions` reply that `Create` itself emits, so the following `Attach`
///   would race against the reaper and intermittently find nothing.
/// * The `read`: printing straight away races against `Attach`. If the text
///   lands first it arrives in the attach *snapshot* and the pump correctly
///   has nothing new to stream, so a test waiting on the pump fails under
///   load. Blocking until [`poke`] makes the output arrive strictly after
///   the attach, which is the ordering these tests are about.
fn printing_command(text: &str) -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        format!("read line; printf '{text}'; sleep 30"),
    ]
}

fn sleeping_command() -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()]
}

fn input(id: &str, bytes: &[u8]) -> TerminalChunk {
    TerminalChunk {
        kind: ChunkKind::Input,
        terminal_id: id.to_string(),
        offset: 0,
        data: bytes.to_vec(),
    }
}

/// Sends the newline a [`printing_command`] session is waiting for.
fn poke(bridge: &mut TerminalBridge, registry: &mut TerminalRegistry, id: &str) {
    bridge.handle_chunk(&input(id, b"\n"), registry).unwrap();
}

fn create(
    bridge: &mut TerminalBridge,
    registry: &mut TerminalRegistry,
    command: Option<Vec<String>>,
) -> String {
    let mut control = request(TerminalAction::Create, &bridge.wire);
    control.command = command;
    control.rows = Some(24);
    control.cols = Some(80);
    bridge.handle_control(&control, registry).unwrap();
    match &bridge.pump(DEFAULT_PUMP_BUDGET, registry)[0] {
        TerminalEvent::SendControl(reply) => {
            assert_eq!(reply.action, TerminalAction::Created);
            reply.terminal_id.clone().unwrap()
        }
        other => panic!("expected Created, got {other:?}"),
    }
}

fn attach(
    bridge: &mut TerminalBridge,
    registry: &mut TerminalRegistry,
    id: &str,
) -> Vec<TerminalEvent> {
    let mut control = request(TerminalAction::Attach, &bridge.wire);
    control.terminal_id = Some(id.to_string());
    bridge.handle_control(&control, registry).unwrap();
    bridge.pump(usize::MAX / 2, registry)
}

fn control_request(
    bridge: &mut TerminalBridge,
    registry: &mut TerminalRegistry,
    action: TerminalAction,
    id: &str,
) {
    let mut control = request(action, &bridge.wire);
    control.terminal_id = Some(id.to_string());
    bridge.handle_control(&control, registry).unwrap();
}

fn output_of(events: &[TerminalEvent]) -> Vec<u8> {
    events
        .iter()
        .filter_map(|event| match event {
            TerminalEvent::SendChunk(chunk) => Some(chunk.data.clone()),
            TerminalEvent::SendControl(_) => None,
        })
        .flatten()
        .collect()
}

/// Pumps until the output contains `needle` (or ten seconds pass),
/// returning all output seen. Stopping at the first output instead raced
/// the terminal's echo of the typed line against the program's reply.
fn pump_until_text(
    bridge: &mut TerminalBridge,
    registry: &mut TerminalRegistry,
    needle: &str,
) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        seen.extend(output_of(&bridge.pump(DEFAULT_PUMP_BUDGET, registry)));
        if String::from_utf8_lossy(&seen).contains(needle) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    seen
}

/// Pumps until `wanted` matches an event, returning everything seen.
fn pump_until(
    bridge: &mut TerminalBridge,
    registry: &mut TerminalRegistry,
    wanted: impl Fn(&TerminalEvent) -> bool,
) -> Vec<TerminalEvent> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        seen.extend(bridge.pump(DEFAULT_PUMP_BUDGET, registry));
        if seen.iter().any(&wanted) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    seen
}

fn is_control(action: TerminalAction) -> impl Fn(&TerminalEvent) -> bool {
    move |event| matches!(event, TerminalEvent::SendControl(c) if c.action == action)
}

#[test]
fn listing_reports_created_sessions() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    let id = create(&mut bridge, &mut registry, Some(sleeping_command()));

    bridge
        .handle_control(&request(TerminalAction::List, &bridge.wire), &mut registry)
        .unwrap();
    match &bridge.pump(DEFAULT_PUMP_BUDGET, &mut registry)[0] {
        TerminalEvent::SendControl(reply) => {
            let sessions = reply.sessions.as_ref().unwrap();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].terminal_id, id);
        }
        other => panic!("expected Sessions, got {other:?}"),
    }
    registry.close(&id);
}

#[test]
fn attaching_returns_a_screen_and_then_streams_output() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    let id = create(
        &mut bridge,
        &mut registry,
        Some(printing_command("bridge-hello")),
    );

    // The list first (the size the screen is drawn for), then the screen, then the
    // reply naming where the stream resumes.
    let events = attach(&mut bridge, &mut registry, &id);
    assert!(matches!(
        &events[0],
        TerminalEvent::SendControl(c) if c.action == TerminalAction::Sessions
    ));
    assert!(matches!(&events[1], TerminalEvent::SendChunk(c) if c.kind == ChunkKind::Snapshot));
    assert!(matches!(
        events.last(),
        Some(TerminalEvent::SendControl(c)) if c.action == TerminalAction::Attached
    ));

    poke(&mut bridge, &mut registry, &id);
    let seen = pump_until_text(&mut bridge, &mut registry, "bridge-hello");
    assert!(
        String::from_utf8_lossy(&seen).contains("bridge-hello"),
        "output never streamed: {seen:?}"
    );
    registry.close(&id);
}

/// An open desk window owns the size: the phone's resizes and typing never
/// squeeze it, the phone is told the desk's size to scale, and the phone
/// gets its own size back once the desk window closes.
#[test]
fn an_open_desk_window_owns_the_size_and_the_phone_is_told() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    let id = create(&mut bridge, &mut registry, Some(sleeping_command()));
    attach(&mut bridge, &mut registry, &id);
    let phone_resize = |bridge: &mut TerminalBridge, registry: &mut TerminalRegistry| {
        let mut resize = request(TerminalAction::Resize, &bridge.wire);
        resize.terminal_id = Some(id.clone());
        resize.rows = Some(30);
        resize.cols = Some(50);
        bridge.handle_control(&resize, registry).unwrap();
    };
    phone_resize(&mut bridge, &mut registry);

    let told = |events: Vec<TerminalEvent>| {
        events.into_iter().find_map(|event| match event {
            TerminalEvent::SendControl(c) if c.action == TerminalAction::Sessions => c
                .sessions
                .and_then(|list| list.into_iter().find(|s| s.terminal_id == id))
                .map(|s| (s.rows, s.cols)),
            _ => None,
        })
    };
    assert_eq!(
        told(bridge.pump(usize::MAX / 2, &mut registry)),
        Some((30, 50))
    );

    // A desk window opens at its own size.
    let session = registry.get(&id).unwrap();
    let desk_window = session.desk_window_opened();
    session.desk_window_size(40, 120, true);
    assert_eq!(
        told(bridge.pump(usize::MAX / 2, &mut registry)),
        Some((40, 120))
    );
    assert_eq!(
        told(bridge.pump(usize::MAX / 2, &mut registry)),
        None,
        "told once"
    );

    // The phone's keyboard opens, and it types: the desk keeps its size,
    // and the phone is told it again so it does not draw at its own.
    phone_resize(&mut bridge, &mut registry);
    bridge
        .handle_chunk(&input(&id, b"x"), &mut registry)
        .unwrap();
    assert_eq!(session.size(), (40, 120));
    assert_eq!(
        told(bridge.pump(usize::MAX / 2, &mut registry)),
        Some((40, 120))
    );

    // The desk window closes; the phone's next keystroke takes its size.
    session.desk_window_closed(desk_window);
    bridge
        .handle_chunk(&input(&id, b"x"), &mut registry)
        .unwrap();
    assert_eq!(session.size(), (30, 50));
    assert_eq!(
        told(bridge.pump(usize::MAX / 2, &mut registry)),
        Some((30, 50))
    );
    registry.close(&id);
}

/// The phone that made a session sets its size even though a desk window
/// opens; the desk takes it back by being resized or typed in, the person
/// can hand it over either way, and the claim ends with the phone's attachment.
#[test]
fn the_phone_that_made_the_session_keeps_the_size_until_the_desk_acts() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    let id = create(&mut bridge, &mut registry, Some(sleeping_command()));
    let session = registry.get(&id).unwrap();
    assert!(session.phone_sets_size(), "the creator holds the size");
    attach(&mut bridge, &mut registry, &id);
    let resize = |bridge: &mut TerminalBridge,
                  registry: &mut TerminalRegistry,
                  rows,
                  cols,
                  claim: Option<bool>| {
        let mut resize = request(TerminalAction::Resize, &bridge.wire);
        resize.terminal_id = Some(id.clone());
        resize.rows = Some(rows);
        resize.cols = Some(cols);
        resize.claim_size = claim;
        bridge.handle_control(&resize, registry).unwrap();
    };

    // The window the desk opens for it attaches at its own size, which changes nothing.
    let desk_window = session.desk_window_opened();
    session.desk_window_size(40, 120, false);
    assert!(session.has_desk_window() && !session.desk_sets_size());
    resize(&mut bridge, &mut registry, 30, 50, None);
    assert_eq!(session.size(), (30, 50));

    // Typing or resizing at the desk takes the size back; the phone's plain resizes are then ignored.
    session.desk_window_size(40, 120, true);
    assert_eq!(session.size(), (40, 120));
    assert!(!session.phone_sets_size() && session.desk_sets_size());
    resize(&mut bridge, &mut registry, 30, 50, None);
    assert_eq!(session.size(), (40, 120));

    // The person taps "Phone size", then "Desk size".
    resize(&mut bridge, &mut registry, 30, 50, Some(true));
    assert_eq!(session.size(), (30, 50));
    assert!(session.phone_sets_size());
    resize(&mut bridge, &mut registry, 30, 50, Some(false));
    assert_eq!(session.size(), (40, 120));
    assert!(!session.phone_sets_size());

    // Detaching lets go of a claim too.
    resize(&mut bridge, &mut registry, 30, 50, Some(true));
    let mut detach = request(TerminalAction::Detach, &bridge.wire);
    detach.terminal_id = Some(id.clone());
    bridge.handle_control(&detach, &mut registry).unwrap();
    assert!(!session.phone_sets_size());
    assert_eq!(session.size(), (40, 120));

    // The last window closing leaves the size to the phone.
    session.desk_window_closed(desk_window);
    assert!(!session.has_desk_window());
    registry.close(&id);
}

/// A second phone that takes the keyboard lease must not undo the first
/// phone's size claim by typing.
#[test]
fn typing_does_not_override_another_phones_claim() {
    let mut registry = TerminalRegistry::new();
    let mut first = new_bridge("phone-1");
    let id = create(&mut first, &mut registry, Some(sleeping_command()));
    let session = registry.get(&id).unwrap();
    session.release_phone_claim("phone-1");
    attach(&mut first, &mut registry, &id);
    session.claim_for_phone("phone-1", 30, 50).unwrap();
    // The first phone lets go of the keyboard; the second takes it with a viewport of its own.
    registry.release_terminal_writer(&id, "phone-1");
    let mut second = new_bridge("phone-2");
    attach(&mut second, &mut registry, &id);
    let mut resize = request(TerminalAction::Resize, &second.wire);
    resize.terminal_id = Some(id.clone());
    resize.rows = Some(20);
    resize.cols = Some(40);
    second.handle_control(&resize, &mut registry).unwrap();
    second
        .handle_chunk(&input(&id, b"x"), &mut registry)
        .unwrap();
    assert_eq!(session.size(), (30, 50), "the first phone's claim stands");
    registry.close(&id);
}

/// A viewer (not the keyboard's holder) cannot claim the size.
#[test]
fn only_the_writer_can_claim_the_size() {
    let mut registry = TerminalRegistry::new();
    let mut owner = new_bridge("phone-1");
    let id = create(&mut owner, &mut registry, Some(sleeping_command()));
    let session = registry.get(&id).unwrap();
    session.release_phone_claim("phone-1");
    let desk_window = session.desk_window_opened();
    session.desk_window_size(40, 120, true);
    attach(&mut owner, &mut registry, &id);
    let mut viewer = new_bridge("phone-2");
    attach(&mut viewer, &mut registry, &id);
    let mut claim = request(TerminalAction::Resize, &viewer.wire);
    claim.terminal_id = Some(id.clone());
    claim.rows = Some(30);
    claim.cols = Some(50);
    claim.claim_size = Some(true);
    viewer.handle_control(&claim, &mut registry).unwrap();
    assert!(!session.phone_sets_size());
    assert_eq!(session.size(), (40, 120));
    session.desk_window_closed(desk_window);
    registry.close(&id);
}

/// The pump must not re-send bytes it already sent, or the client renders
/// every chunk twice.
#[test]
fn the_pump_advances_the_offset_and_does_not_repeat() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    let id = create(&mut bridge, &mut registry, Some(printing_command("abc")));
    attach(&mut bridge, &mut registry, &id);

    poke(&mut bridge, &mut registry, &id);
    // The program's own output, not just the echoed newline that may come
    // first, so the stream is really at rest before it is checked.
    let mut seen = Vec::new();
    for event in pump_until(
        &mut bridge,
        &mut registry,
        |event| matches!(event, TerminalEvent::SendChunk(c) if c.data.ends_with(b"abc")),
    ) {
        seen.push(event);
    }
    assert!(!seen.is_empty(), "the program's output never arrived");
    let settled = bridge.offset_for(&id).unwrap();
    assert!(bridge.pump(DEFAULT_PUMP_BUDGET, &mut registry).is_empty());
    assert_eq!(bridge.offset_for(&id), Some(settled));
    registry.close(&id);
}

/// Backpressure: when the caller's budget is small the pump sends less and
/// leaves the rest in the ring, rather than queueing it somewhere unbounded.
#[test]
fn the_pump_respects_its_budget() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    let id = create(
        &mut bridge,
        &mut registry,
        Some(printing_command("0123456789")),
    );
    attach(&mut bridge, &mut registry, &id);

    poke(&mut bridge, &mut registry, &id);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut first = Vec::new();
    while Instant::now() < deadline && first.is_empty() {
        first = output_of(&bridge.pump(4, &mut registry));
        if first.is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    assert!(!first.is_empty());
    assert!(first.len() <= 4, "budget ignored: sent {}", first.len());

    // What was held back is still there, and arrives in order — four bytes
    // at a time, however long that takes the program to produce it.
    let mut all = first;
    while Instant::now() < deadline && !String::from_utf8_lossy(&all).contains("0123456789") {
        let more = output_of(&bridge.pump(4, &mut registry));
        assert!(more.len() <= 4, "budget ignored: sent {}", more.len());
        all.extend(more);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        String::from_utf8_lossy(&all).contains("0123456789"),
        "held-back output was lost: {all:?}"
    );
    registry.close(&id);
}

/// A 200x200 screen is several frames of snapshot. Attaching used to send
/// every one of them at once, ignoring the channel's buffered amount.
#[test]
fn a_snapshot_is_released_against_the_budget_not_all_at_once() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    let mut control = request(TerminalAction::Create, &bridge.wire);
    control.command = Some(vec![
        "/bin/sh".into(),
        "-c".into(),
        "i=0; while [ $i -lt 200 ]; do printf '%0200d\\n' $i; i=$((i+1)); done; sleep 30".into(),
    ]);
    control.rows = Some(200);
    control.cols = Some(200);
    bridge.handle_control(&control, &mut registry).unwrap();
    let id = match &bridge.pump(DEFAULT_PUMP_BUDGET, &mut registry)[0] {
        TerminalEvent::SendControl(reply) => reply.terminal_id.clone().unwrap(),
        other => panic!("expected Created, got {other:?}"),
    };
    let session = registry.get(&id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while session.head() < 40_000 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut attach_request = request(TerminalAction::Attach, &bridge.wire);
    attach_request.terminal_id = Some(id.clone());
    bridge
        .handle_control(&attach_request, &mut registry)
        .unwrap();
    assert_eq!(bridge.attached.len(), 1);

    // A one-byte budget releases one frame per pump, never the lot.
    let mut frames = 0;
    let mut finished = false;
    for _ in 0..200 {
        let events = bridge.pump(1, &mut registry);
        assert!(
            events.len() <= 1,
            "budget of 1 released {} events",
            events.len()
        );
        frames += events
            .iter()
            .filter(|event| matches!(event, TerminalEvent::SendChunk(_)))
            .count();
        if events.iter().any(is_control(TerminalAction::Attached)) {
            finished = true;
            break;
        }
    }
    assert!(finished, "the attach reply never arrived");
    assert!(frames >= 2, "expected a multi-frame snapshot, got {frames}");
    registry.close(&id);
}

/// Single writer, many viewers: the second device may watch but its
/// keystrokes must never reach the PTY.
#[test]
fn only_the_first_attached_device_may_type() {
    let mut registry = TerminalRegistry::new();
    let mut first = new_bridge("phone-1");
    let mut second = new_bridge("phone-2");

    let id = create(
        &mut first,
        &mut registry,
        Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "read line; printf 'got:%s' \"$line\"".into(),
        ]),
    );
    attach(&mut first, &mut registry, &id);
    attach(&mut second, &mut registry, &id);

    assert!(registry.is_writer(&id, "phone-1"));
    assert!(!registry.is_writer(&id, "phone-2"));

    second
        .handle_chunk(&input(&id, b"nope\n"), &mut registry)
        .unwrap();
    first
        .handle_chunk(&input(&id, b"yes\n"), &mut registry)
        .unwrap();

    let seen = pump_until_text(&mut first, &mut registry, "got:");
    let text = String::from_utf8_lossy(&seen);
    assert!(
        text.contains("got:yes"),
        "writer's input did not land: {text}"
    );
    assert!(
        !text.contains("nope"),
        "viewer's input reached the PTY: {text}"
    );
    registry.close(&id);
}

/// `Detach` used to leave the lease behind, so another device could not type
/// until the first one disconnected altogether.
#[test]
fn detaching_hands_the_keyboard_to_the_next_device() {
    let mut registry = TerminalRegistry::new();
    let mut first = new_bridge("phone-1");
    let mut second = new_bridge("phone-2");
    let id = create(
        &mut first,
        &mut registry,
        Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "read line; printf 'got:%s' \"$line\"".into(),
        ]),
    );
    attach(&mut first, &mut registry, &id);
    attach(&mut second, &mut registry, &id);
    assert!(registry.is_writer(&id, "phone-1"));

    control_request(&mut first, &mut registry, TerminalAction::Detach, &id);
    assert!(
        !registry.is_writer(&id, "phone-1"),
        "the lease outlived Detach"
    );
    assert_eq!(first.attached.len(), 0);

    // The second phone, still attached, can now type.
    second
        .handle_chunk(&input(&id, b"mine\n"), &mut registry)
        .unwrap();
    assert!(registry.is_writer(&id, "phone-2"));
    let seen = pump_until_text(&mut second, &mut registry, "got:");
    assert!(
        String::from_utf8_lossy(&seen).contains("got:mine"),
        "the second phone's input did not land: {seen:?}"
    );
    registry.close(&id);
}

/// Once detached, output the program produces afterwards is not sent.
#[test]
fn after_a_detach_the_pump_streams_nothing_more() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    let id = create(&mut bridge, &mut registry, Some(printing_command("later")));
    attach(&mut bridge, &mut registry, &id);
    control_request(&mut bridge, &mut registry, TerminalAction::Detach, &id);

    // Detaching twice, or something never attached, is not an error.
    control_request(&mut bridge, &mut registry, TerminalAction::Detach, &id);

    let session = registry.get(&id).unwrap();
    session.write_input(b"\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while session.head() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(session.head() > 0, "the program never produced output");
    assert!(bridge.pump(DEFAULT_PUMP_BUDGET, &mut registry).is_empty());
    registry.close(&id);
}

/// Pasting into a program that is not reading must neither block the caller
/// nor bury the phone in errors: the overflow is dropped, and reported once.
#[test]
fn refused_input_is_reported_once_and_never_blocks() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    let id = create(
        &mut bridge,
        &mut registry,
        Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "stty raw -echo; echo ready; sleep 30".into(),
        ]),
    );
    attach(&mut bridge, &mut registry, &id);
    // `ready` is echoed after `stty raw -echo` has run, so once it is there
    // the terminal really is in the mode that pushes back on writers.
    let session = registry.get(&id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while session.head() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(session.head() > 0, "the shell never became ready");

    let started = Instant::now();
    let chunk = input(&id, &vec![b'x'; MAX_TERMINAL_CHUNK_BYTES]);
    for _ in 0..300 {
        bridge.handle_chunk(&chunk, &mut registry).unwrap();
    }
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "handle_chunk blocked for {:?}",
        started.elapsed()
    );
    let refusals = bridge
        .pump(usize::MAX / 2, &mut registry)
        .into_iter()
        .filter(|event| matches!(event, TerminalEvent::SendControl(c) if c.action == TerminalAction::Error))
        .count();
    // Not exactly one: a write that succeeds re-arms the report, and a busy
    // machine can let the writer thread free a queue slot mid-burst. What
    // matters is that the phone is told, and not once per dropped chunk
    // (about 235 here).
    assert!(refusals >= 1, "the phone was never told input was dropped");
    assert!(refusals < 20, "{refusals} refusals for one burst");
    registry.close(&id);
}

/// A shell that exits by itself must reach the phone promptly, with its
/// status, and after the last thing it printed.
#[test]
fn an_exiting_shell_reports_its_status_after_its_last_output() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    let id = create(
        &mut bridge,
        &mut registry,
        Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "read line; printf 'goodbye'; exit 3".into(),
        ]),
    );
    attach(&mut bridge, &mut registry, &id);
    poke(&mut bridge, &mut registry, &id);

    let events = pump_until(
        &mut bridge,
        &mut registry,
        is_control(TerminalAction::Closed),
    );
    let closed_at = events
        .iter()
        .position(is_control(TerminalAction::Closed))
        .expect("the phone was never told the shell exited");
    match &events[closed_at] {
        TerminalEvent::SendControl(closed) => {
            assert_eq!(closed.terminal_id.as_deref(), Some(id.as_str()));
            assert_eq!(closed.exit_status, Some(3));
        }
        other => panic!("{other:?}"),
    }
    let before = output_of(&events[..closed_at]);
    assert!(
        String::from_utf8_lossy(&before).contains("goodbye"),
        "closed arrived before the shell's last output: {before:?}"
    );
    assert_eq!(bridge.attached.len(), 0, "an ended terminal stays attached");
    assert!(!registry.is_writer(&id, "phone-1"));
}

/// The exit is still reported when another device's `list` or a `create`
/// reaped the session from the registry first.
#[test]
fn the_exit_is_reported_even_if_the_registry_reaped_the_session() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    let id = create(
        &mut bridge,
        &mut registry,
        Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "read line; exit 4".into(),
        ]),
    );
    attach(&mut bridge, &mut registry, &id);
    poke(&mut bridge, &mut registry, &id);

    let deadline = Instant::now() + Duration::from_secs(10);
    while registry.reap().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(registry.get(&id).is_none(), "the session should be reaped");

    let events = pump_until(
        &mut bridge,
        &mut registry,
        is_control(TerminalAction::Closed),
    );
    assert!(events.iter().any(|event| matches!(
        event,
        TerminalEvent::SendControl(c)
            if c.action == TerminalAction::Closed && c.exit_status == Some(4)
    )));
}

/// The session list used to reap before listing, so `exit_status` was never
/// anything but `None`.
#[test]
fn a_finished_session_is_listed_once_with_its_status() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    let id = create(
        &mut bridge,
        &mut registry,
        Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "read line; exit 5".into(),
        ]),
    );
    // Let it exit only now, so nothing has reaped it before this test looks.
    let session = registry.get(&id).unwrap();
    session.write_input(b"\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while session.exit_status().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    bridge
        .handle_control(&request(TerminalAction::List, &bridge.wire), &mut registry)
        .unwrap();
    let listed = match &bridge.pump(DEFAULT_PUMP_BUDGET, &mut registry)[0] {
        TerminalEvent::SendControl(reply) => reply.sessions.clone().unwrap(),
        other => panic!("expected Sessions, got {other:?}"),
    };
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].exit_status, Some(5));

    // Having been reported, it is gone.
    bridge
        .handle_control(&request(TerminalAction::List, &bridge.wire), &mut registry)
        .unwrap();
    match &bridge.pump(DEFAULT_PUMP_BUDGET, &mut registry)[0] {
        TerminalEvent::SendControl(reply) => {
            assert!(reply.sessions.as_ref().unwrap().is_empty())
        }
        other => panic!("expected Sessions, got {other:?}"),
    }
}

/// Turning the setting off must cut the phone off at once — without ending
/// the sessions, which belong to the desktop user.
#[test]
fn revoking_detaches_frees_the_lease_and_leaves_the_session_running() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    let id = create(&mut bridge, &mut registry, Some(printing_command("secret")));
    attach(&mut bridge, &mut registry, &id);
    assert!(registry.is_writer(&id, "phone-1"));

    // Output is already on its way when access is withdrawn.
    poke(&mut bridge, &mut registry, &id);
    std::thread::sleep(Duration::from_millis(150));
    bridge.revoke(&mut registry);

    assert_eq!(bridge.attached.len(), 0);
    assert!(!registry.is_writer(&id, "phone-1"), "the lease survived");
    let events = bridge.pump(DEFAULT_PUMP_BUDGET, &mut registry);
    assert!(
        output_of(&events).is_empty(),
        "output kept streaming after revocation"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        TerminalEvent::SendControl(c)
            if c.action == TerminalAction::Closed
                && c.terminal_id.as_deref() == Some(id.as_str())
                && c.error.as_deref() == Some(TERMINAL_NOT_ALLOWED)
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        TerminalEvent::SendControl(c)
            if c.action == TerminalAction::Error
                && c.error.as_deref() == Some(TERMINAL_NOT_ALLOWED)
    )));

    // Nothing more is ever streamed to it, and typing goes nowhere.
    assert!(bridge.pump(DEFAULT_PUMP_BUDGET, &mut registry).is_empty());
    bridge
        .handle_chunk(&input(&id, b"still here?\n"), &mut registry)
        .unwrap();

    // The session is still running, still listed, and another device can
    // pick it up.
    let session = registry.get(&id).expect("revocation ended the session");
    assert!(session.exit_status().is_none());
    assert!(!session.is_closed());
    let mut other = new_bridge("phone-2");
    attach(&mut other, &mut registry, &id);
    assert!(registry.is_writer(&id, "phone-2"));
    registry.close(&id);
}

/// The host never accepts output or snapshot frames from a client — those
/// are host-authored, and honouring one would let a peer forge screen state.
#[test]
fn the_host_refuses_client_authored_output() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    for kind in [ChunkKind::Output, ChunkKind::Snapshot] {
        let forged = TerminalChunk {
            kind,
            terminal_id: "abc".to_string(),
            offset: 0,
            data: b"x".to_vec(),
        };
        assert!(bridge.handle_chunk(&forged, &mut registry).is_err());
    }
}

/// Same rule for control: a peer replaying a host-authored action is not
/// making a request.
#[test]
fn the_host_refuses_host_authored_actions() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    for action in [
        TerminalAction::Sessions,
        TerminalAction::Created,
        TerminalAction::Attached,
        TerminalAction::Closed,
        TerminalAction::Error,
    ] {
        assert!(
            bridge
                .handle_control(&request(action, &bridge.wire), &mut registry)
                .is_err(),
            "{action:?} should be refused"
        );
    }
}

#[test]
fn attaching_to_an_unknown_session_reports_an_error() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    let events = attach(
        &mut bridge,
        &mut registry,
        "00000000-0000-0000-0000-000000000000",
    );
    match &events[0] {
        TerminalEvent::SendControl(reply) => {
            assert_eq!(reply.action, TerminalAction::Error);
            assert!(reply.error.is_some());
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn an_oversized_payload_is_split_into_wire_legal_frames() {
    let chunk = TerminalChunk {
        kind: ChunkKind::Snapshot,
        terminal_id: "abc".to_string(),
        offset: 100,
        data: vec![7; MAX_TERMINAL_CHUNK_BYTES * 2 + 5],
    };
    let events = split_chunk_events(chunk);
    assert_eq!(events.len(), 3);
    let mut expected_offset = 100;
    for event in &events {
        let TerminalEvent::SendChunk(chunk) = event else {
            panic!("expected chunks");
        };
        assert!(chunk.data.len() <= MAX_TERMINAL_CHUNK_BYTES);
        assert_eq!(chunk.offset, expected_offset);
        assert!(chunk.encode().is_ok(), "split frame must be wire-legal");
        expected_offset += chunk.data.len() as u64;
    }
}

/// `title` comes from a client-supplied argv[0] and rides in every session
/// list. Unbounded, thirty-two of them made a list larger than an envelope
/// payload, which the pump could not send — and it stopped. Titles are cut
/// to `MAX_TITLE_CHARS` (see `pty::session`), which this pins the result of.
#[test]
fn the_biggest_possible_session_list_fits_an_envelope() {
    use crate::device_links::pty::session::MAX_TITLE_CHARS;

    let wire = wire("phone-1");
    let mut message = TerminalControl::new(TerminalAction::Sessions, &wire);
    message.sessions = Some(
        (0..crate::device_links::pty::registry::MAX_SESSIONS)
            .map(|_| TerminalSessionInfo {
                terminal_id: Uuid::new_v4().to_string(),
                // Multi-byte characters: the limit is in characters, the
                // payload in bytes.
                title: "\u{10348}".repeat(MAX_TITLE_CHARS),
                rows: 1000,
                cols: 1000,
                exit_status: Some(u32::MAX),
                desk_window: true,
                phone_size: true,
                attached: true,
                cwd: Some(
                    "\u{10348}".repeat(crate::device_links::pty::registry::MAX_REPORTED_CWD / 4),
                ),
                started_at: Some(u64::MAX),
                shell: Some("\u{10348}".repeat(4)),
            })
            .collect(),
    );
    message.folders = Some(vec![
        "/".repeat(
            crate::device_links::pty::registry::MAX_REPORTED_CWD
        );
        8
    ]);
    let payload = message.to_json().unwrap();
    let envelope = WebRtcEnvelope::create(
        &wire,
        WebRtcChannel::Terminal,
        TERMINAL_CONTROL_MESSAGE_TYPE,
        &payload,
        1,
    )
    .unwrap();
    let length = envelope.to_json().unwrap().len();
    assert!(
        length <= crate::device_links::webrtc::MAX_WIRE_MESSAGE_BYTES,
        "one data-channel message at most: {length}"
    );
}

// ---- the connection lifecycle: reconnect races and the idle reaper ----

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

fn wire_attempt(peer: &str) -> WebRtcWireBinding {
    WebRtcWireBinding::from_attempt("desktop-1", peer, &Uuid::new_v4().to_string()).unwrap()
}

fn attach_through(wire: &WebRtcWireBinding, id: &str) {
    let mut control = request(TerminalAction::Attach, wire);
    control.terminal_id = Some(id.to_string());
    with_bridge(wire, |bridge, registry| {
        bridge.handle_control(&control, registry)
    })
    .unwrap();
}

fn create_through(wire: &WebRtcWireBinding) -> String {
    let mut control = request(TerminalAction::Create, wire);
    control.command = Some(sleeping_command());
    with_bridge(wire, |bridge, registry| {
        bridge.handle_control(&control, registry)
    })
    .unwrap();
    let events = pump_once(
        &wire.peer_device_id,
        current_token(wire),
        DEFAULT_PUMP_BUDGET,
    )
    .expect("bridge");
    match &events[0] {
        TerminalEvent::SendControl(reply) => reply.terminal_id.clone().unwrap(),
        other => panic!("expected Created, got {other:?}"),
    }
}

fn current_token(wire: &WebRtcWireBinding) -> u64 {
    bridges().lock().unwrap()[&wire.peer_device_id].token
}

fn close_globally(id: &str) {
    registry().lock().unwrap().close(id);
}

/// The old pump thread notices it has been replaced a tick after the new
/// connection attached, and its cleanup — keyed by device id — detached
/// the *new* attachment and freed its lease.
#[test]
fn a_stale_pumps_cleanup_cannot_detach_the_newer_connection() {
    let phone = unique("reconnect");
    let old_wire = wire_attempt(&phone);
    let old_token = begin(&old_wire);
    let id = create_through(&old_wire);
    attach_through(&old_wire, &id);
    assert_eq!(attached_count(&phone), 1);

    // The phone reconnects and re-attaches.
    let new_wire = wire_attempt(&phone);
    let new_token = begin(&new_wire);
    assert_ne!(old_token, new_token);
    attach_through(&new_wire, &id);
    assert_eq!(attached_count(&phone), 1);
    assert!(registry().lock().unwrap().is_writer(&id, &phone));

    // The old pump thread, only now, finds out and cleans up after itself.
    assert!(pump_once(&phone, old_token, DEFAULT_PUMP_BUDGET).is_none());
    end(&phone, old_token);

    assert_eq!(attached_count(&phone), 1, "the new attachment was detached");
    assert!(
        registry().lock().unwrap().is_writer(&id, &phone),
        "the new connection lost its write lease"
    );
    assert!(pump_once(&phone, new_token, DEFAULT_PUMP_BUDGET).is_some());

    // Messages still bound to the old connection are refused.
    let stale = with_bridge(&old_wire, |_, _| Ok(()));
    assert!(stale.is_err());

    end(&phone, new_token);
    assert_eq!(attached_count(&phone), 0);
    assert!(!registry().lock().unwrap().is_writer(&id, &phone));
    close_globally(&id);
}

#[test]
fn a_new_connection_starts_with_nothing_attached() {
    let phone = unique("fresh");
    let first = wire_attempt(&phone);
    let token = begin(&first);
    let id = create_through(&first);
    attach_through(&first, &id);

    let second = wire_attempt(&phone);
    begin(&second);
    assert_eq!(
        attached_count(&phone),
        0,
        "a reconnect inherited attachments"
    );
    assert!(!registry().lock().unwrap().is_writer(&id, &phone));
    end(&phone, token); // stale: a no-op
    close_globally(&id);
    end(&phone, current_token(&second));
}

/// What "attached" means to the reaper is what the bridges say. Run against
/// a private registry and bridge map: the sweep is indiscriminate by
/// design, and the process-wide ones are shared with every other test.
#[test]
fn the_idle_reaper_closes_forgotten_sessions_but_not_watched_ones() {
    let mut registry = TerminalRegistry::new();
    let mut bridge = new_bridge("phone-1");
    let watched = create(&mut bridge, &mut registry, Some(sleeping_command()));
    let forgotten = create(&mut bridge, &mut registry, Some(sleeping_command()));
    attach(&mut bridge, &mut registry, &watched);
    let open = HashMap::from([("phone-1".to_string(), bridge)]);

    // Recent activity keeps even an unattended session.
    assert!(sweep_in(&open, &mut registry, Duration::from_secs(3600)).is_empty());

    let closed = sweep_in(&open, &mut registry, Duration::ZERO);
    assert_eq!(closed, vec![forgotten.clone()]);
    assert!(
        registry.get(&watched).is_some(),
        "a watched session was closed"
    );
    assert!(registry.get(&forgotten).is_none());
    registry.close(&watched);
}

// ---- process_envelope: consent, handover and binding gates ----

/// A paired phone's connection as `process_envelope` sees it.
struct Connected {
    sessions: Arc<DeviceManager>,
    binding: SessionBinding,
    attempt_id: String,
    /// The desktop's view of the wire, as the coordinator holds it.
    wire: WebRtcWireBinding,
    /// The phone's view of the same wire, which its envelopes are built from.
    phone_wire: WebRtcWireBinding,
    token: u64,
}

fn connect(phone: &str, features_ready: bool) -> Connected {
    let sessions = Arc::new(DeviceManager::new());
    let binding = sessions
        .register_link(
            phone.to_string(),
            SessionLink::test_placeholder(phone),
            true,
        )
        .unwrap()
        .binding;
    let attempt_id = Uuid::new_v4().to_string();
    let wire = WebRtcWireBinding::from_attempt("desktop-1", phone, &attempt_id).unwrap();
    sessions
        .with_session(&binding, |session| {
            let mut handover = HandoverRuntime::new(attempt_id.clone(), wire.clone());
            if features_ready {
                handover.record_offer("offer".to_string()).unwrap();
                handover.record_answer("answer".to_string()).unwrap();
                handover.mark_local_authenticated().unwrap();
                handover.mark_remote_authenticated().unwrap();
                handover.mark_local_capabilities_confirmed().unwrap();
                handover.mark_remote_capabilities_confirmed().unwrap();
                handover.mark_local_feature_ready().unwrap();
                handover.mark_remote_feature_ready().unwrap();
            }
            session.webrtc_attempt_id = Some(attempt_id.clone());
            session.webrtc_handover = Some(handover);
        })
        .unwrap();
    let phone_wire = WebRtcWireBinding {
        sender_device_id: phone.to_string(),
        peer_device_id: "desktop-1".to_string(),
        session_id: wire.session_id,
        generation: wire.generation,
    };
    let token = begin(&wire);
    Connected {
        sessions,
        binding,
        attempt_id,
        wire,
        phone_wire,
        token,
    }
}

impl Connected {
    fn control_envelope(&self, action: TerminalAction, id: Option<&str>) -> WebRtcEnvelope {
        let mut control = TerminalControl::new(action, &self.phone_wire);
        control.terminal_id = id.map(str::to_string);
        control.command = (action == TerminalAction::Create).then(sleeping_command);
        WebRtcEnvelope::create(
            &self.phone_wire,
            WebRtcChannel::Terminal,
            TERMINAL_CONTROL_MESSAGE_TYPE,
            &control.to_json().unwrap(),
            1,
        )
        .unwrap()
    }

    fn chunk_envelope(&self, id: &str, bytes: &[u8]) -> WebRtcEnvelope {
        WebRtcEnvelope::create(
            &self.phone_wire,
            WebRtcChannel::Terminal,
            TERMINAL_CHUNK_MESSAGE_TYPE,
            &input(id, bytes).encode().unwrap(),
            1,
        )
        .unwrap()
    }

    fn deliver(&self, envelope: WebRtcEnvelope) -> Result<(), String> {
        process_envelope(
            &self.binding,
            &self.attempt_id,
            &self.wire,
            envelope,
            &self.sessions,
        )
    }

    fn replies(&self) -> Vec<TerminalEvent> {
        pump_once(&self.wire.peer_device_id, self.token, DEFAULT_PUMP_BUDGET).unwrap_or_default()
    }

    fn sessions_owned(&self) -> usize {
        registry()
            .lock()
            .unwrap()
            .owned_by(&self.wire.peer_device_id)
    }
}

/// Turns the setting on or off the way the settings switch does: through a
/// real `Config`, which is also what keeps the process-wide view in step.
fn set_consent(phone: &str, allowed: bool) {
    let dir = std::env::temp_dir().join(format!("desklink-consent-{}", Uuid::new_v4()));
    let mut config = Config::load_from_dir(dir.clone()).unwrap();
    config
        .trust_device(
            &DeviceInfo::local(phone.to_string(), "Test phone".to_string()),
            "cert".to_string(),
        )
        .unwrap();
    config.set_terminal_access(phone, allowed).unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

fn error_of(events: &[TerminalEvent]) -> Option<String> {
    events.iter().find_map(|event| match event {
        TerminalEvent::SendControl(c) if c.action == TerminalAction::Error => c.error.clone(),
        _ => None,
    })
}

/// The default: a paired phone that has not been allowed a terminal gets
/// told so, and nothing is created.
#[test]
fn a_phone_without_consent_is_denied_and_creates_nothing() {
    let phone = unique("denied");
    let connection = connect(&phone, true);
    let before = connection.sessions_owned();

    connection
        .deliver(connection.control_envelope(TerminalAction::Create, None))
        .unwrap();
    connection
        .deliver(connection.control_envelope(TerminalAction::List, None))
        .unwrap();

    let events = connection.replies();
    assert_eq!(
        error_of(&events).as_deref(),
        Some("terminal-not-allowed"),
        "{events:?}"
    );
    // Both requests were answered with a denial; neither did any work.
    assert_eq!(
        events
            .iter()
            .filter(
                |e| matches!(e, TerminalEvent::SendControl(c) if c.action == TerminalAction::Error)
            )
            .count(),
        2
    );
    assert!(!events.iter().any(is_control(TerminalAction::Created)));
    assert!(!events.iter().any(is_control(TerminalAction::Sessions)));
    assert_eq!(connection.sessions_owned(), before);
    end(&phone, connection.token);
}

#[test]
fn a_phone_with_consent_can_create_and_type() {
    let phone = unique("allowed");
    set_consent(&phone, true);
    let connection = connect(&phone, true);

    connection
        .deliver(connection.control_envelope(TerminalAction::Create, None))
        .unwrap();
    let events = connection.replies();
    let id = events
        .iter()
        .find_map(|event| match event {
            TerminalEvent::SendControl(c) if c.action == TerminalAction::Created => {
                c.terminal_id.clone()
            }
            _ => None,
        })
        .expect("Created");
    assert!(error_of(&events).is_none(), "{events:?}");

    connection
        .deliver(connection.control_envelope(TerminalAction::Attach, Some(&id)))
        .unwrap();
    connection
        .deliver(connection.chunk_envelope(&id, b"hello\n"))
        .unwrap();
    assert_eq!(attached_count(&phone), 1);
    assert!(registry().lock().unwrap().is_writer(&id, &phone));

    end(&phone, connection.token);
    close_globally(&id);
}

/// Consent is checked on every message, not remembered from the last one:
/// a chunk from a phone whose access was withdrawn must not reach the PTY,
/// even if it is somehow still attached.
#[test]
fn input_from_a_phone_whose_consent_is_gone_never_reaches_the_pty() {
    let phone = unique("withdrawn");
    set_consent(&phone, true);
    let connection = connect(&phone, true);
    connection
        .deliver(connection.control_envelope(TerminalAction::Create, None))
        .unwrap();
    let id = connection
        .replies()
        .iter()
        .find_map(|event| match event {
            TerminalEvent::SendControl(c) if c.action == TerminalAction::Created => {
                c.terminal_id.clone()
            }
            _ => None,
        })
        .unwrap();
    connection
        .deliver(connection.control_envelope(TerminalAction::Attach, Some(&id)))
        .unwrap();
    connection.replies();
    let session = registry().lock().unwrap().get(&id).unwrap();

    // Consent goes away with the phone still attached (the setting was
    // written but `revoke_device` has not run yet).
    set_consent(&phone, false);
    let idle = session.idle_for();
    std::thread::sleep(Duration::from_millis(50));
    connection
        .deliver(connection.chunk_envelope(&id, b"rm -rf ~\n"))
        .unwrap();
    assert!(
        session.idle_for() >= idle,
        "input was written after consent was withdrawn"
    );

    end(&phone, connection.token);
    close_globally(&id);
}

#[test]
fn revoke_device_cuts_off_an_attached_phone_and_keeps_its_sessions() {
    let phone = unique("revoked");
    set_consent(&phone, true);
    let connection = connect(&phone, true);
    connection
        .deliver(connection.control_envelope(TerminalAction::Create, None))
        .unwrap();
    let id = connection
        .replies()
        .iter()
        .find_map(|event| match event {
            TerminalEvent::SendControl(c) if c.action == TerminalAction::Created => {
                c.terminal_id.clone()
            }
            _ => None,
        })
        .unwrap();
    connection
        .deliver(connection.control_envelope(TerminalAction::Attach, Some(&id)))
        .unwrap();
    connection.replies();
    assert_eq!(attached_count(&phone), 1);

    set_consent(&phone, false);
    revoke_device(&phone);

    assert_eq!(attached_count(&phone), 0);
    assert!(!registry().lock().unwrap().is_writer(&id, &phone));
    let events = connection.replies();
    assert!(events.iter().any(is_control(TerminalAction::Closed)));
    assert_eq!(error_of(&events).as_deref(), Some("terminal-not-allowed"));
    assert!(
        registry().lock().unwrap().get(&id).is_some(),
        "revoking access ended the session"
    );

    // And it stays cut off.
    connection
        .deliver(connection.control_envelope(TerminalAction::Attach, Some(&id)))
        .unwrap();
    assert_eq!(attached_count(&phone), 0);
    end(&phone, connection.token);
    close_globally(&id);
}

#[test]
fn a_connection_that_has_not_finished_the_handover_is_rejected() {
    let phone = unique("unready");
    set_consent(&phone, true);
    let connection = connect(&phone, false);
    let before = connection.sessions_owned();

    assert!(connection
        .deliver(connection.control_envelope(TerminalAction::Create, None))
        .is_err());
    assert!(connection
        .deliver(connection.chunk_envelope("abc", b"x"))
        .is_err());
    assert_eq!(connection.sessions_owned(), before);
    end(&phone, connection.token);
}

/// A binding that has been replaced must be refused for input chunks as
/// much as for control messages; chunks used to skip the check.
#[test]
fn a_stale_binding_is_rejected_for_controls_and_for_chunks() {
    let phone = unique("stale");
    set_consent(&phone, true);
    let connection = connect(&phone, true);
    // The transport is replaced under the connection.
    connection
        .sessions
        .register_link(phone.clone(), SessionLink::test_placeholder(&phone), true)
        .unwrap();
    assert!(!connection.sessions.is_current(&connection.binding));

    assert!(connection
        .deliver(connection.control_envelope(TerminalAction::Create, None))
        .is_err());
    assert!(connection
        .deliver(connection.chunk_envelope("abc", b"x"))
        .is_err());
    assert_eq!(attached_count(&phone), 0);
    end(&phone, connection.token);
}

#[test]
fn an_envelope_on_the_wrong_channel_is_rejected() {
    let phone = unique("channel");
    set_consent(&phone, true);
    let connection = connect(&phone, true);
    let mut envelope = connection.control_envelope(TerminalAction::List, None);
    envelope.channel = WebRtcChannel::Events.label().to_string();
    assert!(connection.deliver(envelope).is_err());
    end(&phone, connection.token);
}

/// A pump with nothing attached and nothing queued is idle: it sleeps on
/// the wake counter instead of polling every 8 ms for as long as the peer
/// lives; a request, a reply or an alert makes it busy and wakes it.
#[test]
fn an_idle_terminal_pump_sleeps_until_something_touches_the_bridge() {
    let phone = format!("pump-idle-{}", Uuid::new_v4().simple());
    let wire = wire(&phone);
    let token = begin(&wire);
    assert_eq!(pump_is_idle(&phone, token), Some(true));
    // Nothing wakes it: the wait lasts the whole timeout. (Other tests
    // touch bridges and write to terminals at the same time, which wakes
    // every sleeper: one quiet wait in many tries is the proof.)
    let mut seen = wake_generation();
    let mut slept = false;
    for _ in 0..400 {
        seen = wake_generation();
        let started = Instant::now();
        wait_for_work(seen, Duration::from_millis(100));
        if started.elapsed() >= Duration::from_millis(90) {
            slept = true;
            break;
        }
    }
    assert!(slept, "the wait never lasted its timeout");
    // A denial is queued on the bridge: busy, and the sleeper is woken.
    let sleeper = std::thread::spawn(move || {
        let started = Instant::now();
        wait_for_work(seen, Duration::from_secs(10));
        started.elapsed()
    });
    std::thread::sleep(Duration::from_millis(50));
    with_open_bridge(&wire, |bridge| bridge.deny(None)).unwrap();
    assert!(
        sleeper.join().unwrap() < Duration::from_secs(2),
        "not woken"
    );
    assert_eq!(pump_is_idle(&phone, token), Some(false));
    // The pump drains it and is idle again.
    assert!(!pump_once(&phone, token, 1 << 20).unwrap().is_empty());
    assert_eq!(pump_is_idle(&phone, token), Some(true));
    // Superseded: no answer, so its thread ends.
    let newer = begin(&wire);
    assert_eq!(pump_is_idle(&phone, token), None);
    end(&phone, newer);
}
