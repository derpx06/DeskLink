//! GNOME RemoteDesktop portal ownership for DeskLink WebRTC control leases.
//!
//! This module intentionally has no protocol or GTK state.  The session core
//! authorizes a generation-bound lease first, then this adapter owns exactly
//! one portal session for that lease.  Raw pointer packets can only enqueue
//! input to an already granted lease; they can never trigger a portal prompt.

use std::collections::{HashMap, HashSet};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use ashpd::desktop::remote_desktop::{DeviceType, KeyState, RemoteDesktop};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::PersistMode;
use enumflags2::make_bitflags;
use uuid::Uuid;

use crate::device_links::core::SessionBinding;
use crate::device_links::packet::NetworkPacket;

#[derive(Debug)]
pub struct PortalLeaseReady {
    pub restore_token: Option<String>,
    /// The PipeWire remote and selected stream are valid only while this
    /// portal lease is alive. The WebRTC peer takes ownership of the fd and
    /// drops it before the lease closes.
    pub screen_capture: Option<PortalScreenCapture>,
}

/// A single selected desktop monitor from the portal. The node ID is scoped
/// to this live portal session; it is never persisted or reused after the
/// session closes.
#[derive(Debug)]
pub struct PortalScreenCapture {
    pub pipewire_remote: OwnedFd,
    pub node_id: u32,
    pub logical_size: Option<(i32, i32)>,
}

#[derive(Debug)]
enum PortalInput {
    RelativeMotion { dx: f64, dy: f64 },
    AbsoluteMotion { x: f64, y: f64 },
    Button { code: i32, state: KeyState },
    Axis { dx: f64, dy: f64 },
    Key { keysym: i32, state: KeyState },
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalLeaseKind {
    /// A combined ScreenCast + RemoteDesktop session for desktop-to-phone
    /// viewing. It deliberately requests input devices too, but DeskLink does
    /// not inject anything until a separate authenticated control lease is
    /// granted.
    DesktopView,
}

/// A thread-safe handle to one already-authorized portal session.  The portal
/// lives on its own Tokio runtime thread because its D-Bus session object must
/// outlive every input callback.
#[derive(Clone)]
pub struct PortalInputLease {
    id: String,
    sender: Sender<PortalInput>,
    active: Arc<AtomicBool>,
    closed: Arc<Mutex<Option<Receiver<()>>>>,
}

impl PortalInputLease {
    /// Starts a user-visible permission request. This must be called from a
    /// remote-session control request, never from packet motion or key input.
    pub fn request(
        restore_token: Option<String>,
        kind: PortalLeaseKind,
    ) -> Result<(Self, Receiver<Result<PortalLeaseReady, String>>), String> {
        if std::env::var("XDG_SESSION_TYPE")
            .map(|value| value.eq_ignore_ascii_case("x11"))
            .unwrap_or(false)
        {
            return Err(
                "DeskLink X11 fallback is not enabled for this portal-only control lease"
                    .to_string(),
            );
        }

        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (closed_sender, closed_receiver) = mpsc::sync_channel(1);
        let active = Arc::new(AtomicBool::new(false));
        let worker_active = Arc::clone(&active);
        thread::Builder::new()
            .name("DeskLink-RemoteDesktop-Portal".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_sender.send(Err(format!(
                            "Could not start the DeskLink portal runtime: {error}"
                        )));
                        return;
                    }
                };
                let portal = match runtime.block_on(RemoteDesktop::new()) {
                    Ok(portal) => portal,
                    Err(error) => {
                        let _ = ready_sender.send(Err(format!(
                            "GNOME RemoteDesktop portal is unavailable: {error}"
                        )));
                        return;
                    }
                };
                let screencast = match kind {
                    PortalLeaseKind::DesktopView => match runtime.block_on(Screencast::new()) {
                        Ok(portal) => portal,
                        Err(error) => {
                            let _ = ready_sender.send(Err(format!(
                                "GNOME ScreenCast portal is unavailable: {error}"
                            )));
                            return;
                        }
                    },
                };
                let session = match runtime.block_on(portal.create_session()) {
                    Ok(session) => session,
                    Err(error) => {
                        let _ = ready_sender.send(Err(format!(
                            "Could not create the DeskLink remote-control session: {error}"
                        )));
                        return;
                    }
                };
                let device_types = make_bitflags!(DeviceType::{
                    Keyboard | Pointer | Touchscreen
                });
                let select = match runtime.block_on(portal.select_devices(
                    &session,
                    device_types,
                    restore_token.as_deref(),
                    PersistMode::ExplicitlyRevoked,
                )) {
                    Ok(request) => request,
                    Err(error) => {
                        let _ = ready_sender.send(Err(format!(
                            "Could not request DeskLink remote-control devices: {error}"
                        )));
                        return;
                    }
                };
                if let Err(error) = runtime.block_on(select.response()) {
                    let _ = ready_sender.send(Err(format!(
                        "Remote-control permission was denied or cancelled: {error}"
                    )));
                    return;
                }
                {
                    let select_sources = match runtime.block_on(screencast.select_sources(
                        &session,
                        CursorMode::Embedded,
                        make_bitflags!(SourceType::{ Monitor }),
                        false,
                        // RemoteDesktop owns persistence for this combined
                        // session. ScreenCast restore tokens are not valid for
                        // a RemoteDesktop-created session.
                        None,
                        PersistMode::DoNot,
                    )) {
                        Ok(request) => request,
                        Err(error) => {
                            let _ = ready_sender.send(Err(format!(
                                "Could not request the DeskLink desktop screen: {error}"
                            )));
                            return;
                        }
                    };
                    if let Err(error) = runtime.block_on(select_sources.response()) {
                        let _ = ready_sender.send(Err(format!(
                            "Desktop screen-sharing permission was denied or cancelled: {error}"
                        )));
                        return;
                    }
                }
                let start = match runtime.block_on(portal.start(&session, None)) {
                    Ok(request) => request,
                    Err(error) => {
                        let _ = ready_sender.send(Err(format!(
                            "Could not start the DeskLink remote-control session: {error}"
                        )));
                        return;
                    }
                };
                let selected = match runtime.block_on(start.response()) {
                    Ok(selected) => selected,
                    Err(error) => {
                        let _ = ready_sender.send(Err(format!(
                            "Remote-control permission was denied or cancelled: {error}"
                        )));
                        return;
                    }
                };

                let screen_capture = {
                    let stream = match selected.streams().and_then(|streams| streams.first()) {
                        Some(stream) => stream,
                        None => {
                            let _ = ready_sender.send(Err(
                                "The desktop portal did not provide a monitor stream".to_string(),
                            ));
                            return;
                        }
                    };
                    let pipewire_remote = match runtime.block_on(screencast.open_pipe_wire_remote(&session)) {
                        Ok(remote) => remote,
                        Err(error) => {
                            let _ = ready_sender.send(Err(format!(
                                "Could not open the DeskLink PipeWire screen stream: {error}"
                            )));
                            return;
                        }
                    };
                    PortalScreenCapture {
                        pipewire_remote,
                        // ashpd 0.12 exposes the portal node identifier, which
                        // is safe only for this still-open session. A newer
                        // portal binding can supply pipewire-serial; DeskLink
                        // never stores this value or reuses it after closure.
                        node_id: stream.pipe_wire_node_id(),
                        logical_size: stream.size(),
                    }
                };
                let absolute_stream = (
                    screen_capture.node_id,
                    screen_capture.logical_size.unwrap_or((1280, 720)),
                );

                worker_active.store(true, Ordering::Release);
                let _ = ready_sender.send(Ok(PortalLeaseReady {
                    restore_token: selected.restore_token().map(ToOwned::to_owned),
                    screen_capture: Some(screen_capture),
                }));

                let mut pressed_buttons = HashSet::new();
                let mut pressed_keys = HashSet::new();
                while worker_active.load(Ordering::Acquire) {
                    let Ok(command) = receiver.recv() else {
                        break;
                    };
                    let result = match command {
                        PortalInput::RelativeMotion { dx, dy } => runtime.block_on(
                            portal.notify_pointer_motion(&session, dx, dy),
                        ),
                        PortalInput::AbsoluteMotion { x, y } => {
                            let (stream, (width, height)) = absolute_stream;
                            // The WebRTC sender normalizes desktop capture to
                            // 1280×720. Map Android's aspect-fit coordinates
                            // back into the portal stream's logical space.
                            let mapped_x = x.clamp(0.0, 1280.0) * f64::from(width) / 1280.0;
                            let mapped_y = y.clamp(0.0, 720.0) * f64::from(height) / 720.0;
                            runtime.block_on(portal.notify_pointer_motion_absolute(
                                &session,
                                stream,
                                mapped_x,
                                mapped_y,
                            ))
                        }
                        PortalInput::Button { code, state } => {
                            if state == KeyState::Pressed {
                                pressed_buttons.insert(code);
                            } else {
                                pressed_buttons.remove(&code);
                            }
                            runtime.block_on(portal.notify_pointer_button(&session, code, state))
                        }
                        PortalInput::Axis { dx, dy } => runtime.block_on(
                            portal.notify_pointer_axis(&session, dx, dy, true),
                        ),
                        PortalInput::Key { keysym, state } => {
                            if state == KeyState::Pressed {
                                pressed_keys.insert(keysym);
                            } else {
                                pressed_keys.remove(&keysym);
                            }
                            runtime.block_on(portal.notify_keyboard_keysym(&session, keysym, state))
                        }
                        PortalInput::Close => break,
                    };
                    if result.is_err() {
                        // The portal session has been revoked or closed. The
                        // next input is rejected locally; a user must request
                        // control again rather than causing a prompt loop.
                        worker_active.store(false, Ordering::Release);
                        break;
                    }
                }
                worker_active.store(false, Ordering::Release);
                // A lease close must not strand a pressed key or pointer
                // button after a disconnect, expiry, portal revocation, or
                // takeover. Release locally tracked state before closing.
                for code in pressed_buttons {
                    let _ = runtime.block_on(
                        portal.notify_pointer_button(&session, code, KeyState::Released),
                    );
                }
                for keysym in pressed_keys {
                    let _ = runtime.block_on(
                        portal.notify_keyboard_keysym(&session, keysym, KeyState::Released),
                    );
                }
                let _ = runtime.block_on(session.close());
                let _ = closed_sender.send(());
            })
            .map_err(|error| format!("Could not start DeskLink portal worker: {error}"))?;
        Ok((
            Self {
                id: Uuid::new_v4().to_string(),
                sender,
                active,
                closed: Arc::new(Mutex::new(Some(closed_receiver))),
            },
            ready_receiver,
        ))
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// The one closure receiver belongs to the coordinator which installed
    /// this lease. Keeping a single observer avoids duplicate portal-closure
    /// cleanup racing a peer replacement or an explicit user stop.
    pub fn take_closed_receiver(&self) -> Option<Receiver<()>> {
        self.closed.lock().ok()?.take()
    }

    pub fn inject(&self, packet: &NetworkPacket) -> Result<(), String> {
        if !self.active.load(Ordering::Acquire) {
            return Err("DeskLink remote-control portal permission is closed".to_string());
        }
        for command in input_commands(packet)? {
            self.sender
                .send(command)
                .map_err(|_| "DeskLink remote-control portal session is closed".to_string())?;
        }
        Ok(())
    }

    pub fn close(&self) {
        self.active.store(false, Ordering::Release);
        let _ = self.sender.send(PortalInput::Close);
    }
}

#[derive(Default)]
pub struct RemotePortalRegistry {
    leases: Mutex<HashMap<(String, u64, u64), PortalInputLease>>,
}

impl RemotePortalRegistry {
    pub fn global() -> &'static Self {
        static REGISTRY: OnceLock<RemotePortalRegistry> = OnceLock::new();
        REGISTRY.get_or_init(Self::default)
    }

    pub fn install(&self, binding: &SessionBinding, lease: PortalInputLease) -> Result<(), String> {
        let mut leases = self
            .leases
            .lock()
            .map_err(|_| "DeskLink portal registry lock poisoned".to_string())?;
        if let Some(previous) = leases.insert(binding_key(binding), lease) {
            previous.close();
        }
        Ok(())
    }

    pub fn inject(&self, binding: &SessionBinding, packet: &NetworkPacket) -> Result<(), String> {
        let lease = self
            .leases
            .lock()
            .map_err(|_| "DeskLink portal registry lock poisoned".to_string())?
            .get(&binding_key(binding))
            .cloned()
            .ok_or_else(|| "DeskLink remote-control portal lease is not granted".to_string())?;
        lease.inject(packet)
    }

    pub fn contains(&self, binding: &SessionBinding) -> bool {
        self.leases
            .lock()
            .map(|leases| {
                leases
                    .get(&binding_key(binding))
                    .is_some_and(PortalInputLease::is_active)
            })
            .unwrap_or(false)
    }

    /// Checks that a closure observer still refers to the same portal lease.
    /// A newer user-approved screen session may reuse the device/session
    /// binding; an old worker must never pause that newer session.
    pub fn is_current_lease(&self, binding: &SessionBinding, lease_id: &str) -> bool {
        self.leases
            .lock()
            .map(|leases| {
                leases
                    .get(&binding_key(binding))
                    .is_some_and(|lease| lease.id() == lease_id)
            })
            .unwrap_or(false)
    }

    pub fn release(&self, binding: &SessionBinding) {
        if let Ok(mut leases) = self.leases.lock() {
            if let Some(lease) = leases.remove(&binding_key(binding)) {
                lease.close();
            }
        }
    }
}

fn binding_key(binding: &SessionBinding) -> (String, u64, u64) {
    (binding.device_id.clone(), binding.session_id, binding.generation)
}

fn input_commands(packet: &NetworkPacket) -> Result<Vec<PortalInput>, String> {
    let dx = packet.body.get("dx").and_then(|value| value.as_f64()).unwrap_or_default();
    let dy = packet.body.get("dy").and_then(|value| value.as_f64()).unwrap_or_default();
    if !dx.is_finite() || !dy.is_finite() || dx.abs() > 4096.0 || dy.abs() > 4096.0 {
        return Err("DeskLink remote input has an invalid relative movement".to_string());
    }
    let mut commands = Vec::new();
    match (packet.body.get("x"), packet.body.get("y")) {
        (None, None) => {}
        (Some(x), Some(y)) => {
            let x = x
                .as_f64()
                .filter(|value| value.is_finite() && (0.0..=1280.0).contains(value))
                .ok_or_else(|| "DeskLink remote input has an invalid absolute X coordinate".to_string())?;
            let y = y
                .as_f64()
                .filter(|value| value.is_finite() && (0.0..=720.0).contains(value))
                .ok_or_else(|| "DeskLink remote input has an invalid absolute Y coordinate".to_string())?;
            commands.push(PortalInput::AbsoluteMotion { x, y });
        }
        _ => {
            return Err(
                "DeskLink remote input must include both absolute coordinates".to_string(),
            )
        }
    }
    if packet.get_bool("scroll").unwrap_or(false) {
        commands.push(PortalInput::Axis { dx, dy });
    } else if packet.get_bool("singleclick").unwrap_or(false) {
        commands.extend(click(0x110));
    } else if packet.get_bool("doubleclick").unwrap_or(false) {
        commands.extend(click(0x110));
        commands.extend(click(0x110));
    } else if packet.get_bool("middleclick").unwrap_or(false) {
        commands.extend(click(0x112));
    } else if packet.get_bool("rightclick").unwrap_or(false) {
        commands.extend(click(0x111));
    } else if packet.get_bool("singlehold").unwrap_or(false) {
        commands.push(PortalInput::Button { code: 0x110, state: KeyState::Pressed });
    } else if packet.get_bool("singlerelease").unwrap_or(false) {
        commands.push(PortalInput::Button { code: 0x110, state: KeyState::Released });
    } else if packet.get_str("key").is_some() || packet.get_i64("specialKey").unwrap_or_default() > 0 {
        commands.extend(key_commands(packet)?);
    } else if dx != 0.0 || dy != 0.0 {
        commands.push(PortalInput::RelativeMotion { dx, dy });
    }
    if commands.is_empty() {
        return Err("DeskLink remote input packet has no supported action".to_string());
    }
    Ok(commands)
}

fn click(code: i32) -> [PortalInput; 2] {
    [
        PortalInput::Button { code, state: KeyState::Pressed },
        PortalInput::Button { code, state: KeyState::Released },
    ]
}

fn key_commands(packet: &NetworkPacket) -> Result<Vec<PortalInput>, String> {
    let mut commands = Vec::new();
    for (enabled, keysym) in [
        (packet.get_bool("ctrl").unwrap_or(false), 0xffe3),
        (packet.get_bool("alt").unwrap_or(false), 0xffe9),
        (packet.get_bool("shift").unwrap_or(false), 0xffe1),
        (packet.get_bool("super").unwrap_or(false), 0xffeb),
    ] {
        if enabled {
            commands.push(PortalInput::Key { keysym, state: KeyState::Pressed });
        }
    }
    let keysyms = if let Some(code) = packet.get_i64("specialKey").filter(|code| *code > 0) {
        vec![special_keysym(code)
            .ok_or_else(|| "Unsupported DeskLink remote special key".to_string())?]
    } else {
        let text = packet
            .get_str("key")
            .ok_or_else(|| "DeskLink remote key packet has no key".to_string())?;
        if text.is_empty() || text.len() > 4096 {
            return Err("DeskLink remote text input is outside the allowed size".to_string());
        }
        text.chars()
            .map(|character| {
                if character.is_control() {
                    Err("DeskLink remote text contains a control character".to_string())
                } else {
                    Ok(u32::from(character) as i32)
                }
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    for keysym in keysyms {
        commands.push(PortalInput::Key { keysym, state: KeyState::Pressed });
        commands.push(PortalInput::Key { keysym, state: KeyState::Released });
    }
    for (enabled, keysym) in [
        (packet.get_bool("super").unwrap_or(false), 0xffeb),
        (packet.get_bool("shift").unwrap_or(false), 0xffe1),
        (packet.get_bool("alt").unwrap_or(false), 0xffe9),
        (packet.get_bool("ctrl").unwrap_or(false), 0xffe3),
    ] {
        if enabled {
            commands.push(PortalInput::Key { keysym, state: KeyState::Released });
        }
    }
    Ok(commands)
}

fn special_keysym(value: i64) -> Option<i32> {
    Some(match value {
        1 => 0xff08, 2 => 0xff09, 3 | 12 => 0xff0d, 4 => 0xff51,
        5 => 0xff52, 6 => 0xff53, 7 => 0xff54, 8 => 0xff55,
        9 => 0xff56, 10 => 0xff50, 11 => 0xff57, 13 => 0xffff,
        14 => 0xff1b, 21..=32 => 0xffbe + i32::try_from(value - 21).ok()?,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn positioned_click_keeps_absolute_motion_and_button_transition_together() {
        let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_MOUSEPAD_REQUEST);
        packet.set("x", Value::from(640));
        packet.set("y", Value::from(360));
        packet.set("singleclick", Value::Bool(true));

        let commands = input_commands(&packet).unwrap();
        assert!(matches!(
            commands.first(),
            Some(PortalInput::AbsoluteMotion { x, y }) if *x == 640.0 && *y == 360.0
        ));
        assert!(matches!(
            commands.get(1),
            Some(PortalInput::Button { code: 0x110, state: KeyState::Pressed })
        ));
        assert!(matches!(
            commands.get(2),
            Some(PortalInput::Button { code: 0x110, state: KeyState::Released })
        ));
    }

    #[test]
    fn empty_input_packet_is_rejected_before_it_reaches_the_portal() {
        let packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_MOUSEPAD_REQUEST);
        assert!(input_commands(&packet).is_err());
    }

    #[test]
    fn partial_or_out_of_bounds_absolute_coordinates_are_rejected() {
        let mut partial = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_MOUSEPAD_REQUEST);
        partial.set("x", Value::from(1));
        partial.set("singleclick", Value::Bool(true));
        assert!(input_commands(&partial).is_err());

        let mut outside = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_MOUSEPAD_REQUEST);
        outside.set("x", Value::from(1281));
        outside.set("y", Value::from(1));
        outside.set("singleclick", Value::Bool(true));
        assert!(input_commands(&outside).is_err());
    }

    #[test]
    fn printable_text_is_emitted_as_an_ordered_key_sequence() {
        let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_MOUSEPAD_REQUEST);
        packet.set("key", Value::String("Hi".to_string()));

        let commands = input_commands(&packet).unwrap();
        assert!(matches!(
            commands.as_slice(),
            [
                PortalInput::Key { keysym: 72, state: KeyState::Pressed },
                PortalInput::Key { keysym: 72, state: KeyState::Released },
                PortalInput::Key { keysym: 105, state: KeyState::Pressed },
                PortalInput::Key { keysym: 105, state: KeyState::Released },
            ]
        ));
    }
}
