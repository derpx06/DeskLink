//! Input through EIS (libei): the socket a RemoteDesktop portal session
//! hands out with `ConnectToEIS`.
//!
//! Absolute pointer positions go to the region whose `mapping_id` matches a
//! screen stream's (the portal's D-Bus calls refuse absolute positions on a
//! virtual monitor, whose stream has no size). Positions are fractions of
//! that region, so the caller never needs the compositor's layout. Buttons,
//! scroll and keys (evdev codes) go to whichever device offers them.
//!
//! One worker thread owns the connection: it reads the server's events and
//! sends what [`EisInput`] queues. Dropping the handle closes it.

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reis::ei;
use reis::event::{DeviceCapability, EiEvent, EiEventConverter};

/// One input event to send.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Command {
    /// `(x, y)` as fractions (0..=1) of the region mapped to `mapping_id`
    /// (any absolute region when `None`).
    Absolute {
        mapping_id: Option<String>,
        x: f64,
        y: f64,
    },
    Relative(f64, f64),
    /// An evdev button code (BTN_LEFT = 0x110).
    Button(u32, bool),
    /// Smooth scroll in logical pixels; `stop` ends the gesture.
    Scroll {
        dx: f64,
        dy: f64,
        stop: bool,
    },
    /// An evdev key code (KEY_A = 30).
    Key(u32, bool),
}

#[derive(Debug, Default)]
struct Shared {
    connected: bool,
    /// Absolute regions offered: mapping id, whether their device is
    /// resumed, and their logical width.
    regions: Vec<(Option<String>, bool, u32)>,
}

/// A live EIS connection (sender context).
pub(crate) struct EisInput {
    commands: Sender<Command>,
    shared: Arc<Mutex<Shared>>,
}

impl EisInput {
    /// Connects over the portal's socket and starts the worker. Returns once
    /// the handshake is done; devices arrive after.
    pub(crate) fn connect(socket: OwnedFd, name: &str) -> Result<Self, String> {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (commands, receiver) = mpsc::channel();
        let (started, handshake) = mpsc::channel();
        let (worker_shared, name) = (Arc::clone(&shared), name.to_string());
        // The connection's types are not `Send`: it is made and used on the
        // worker alone.
        std::thread::Builder::new()
            .name("desklink-eis".into())
            .spawn(move || {
                let opened = ei::Context::new(UnixStream::from(socket))
                    .map_err(|error| format!("Could not open the input connection: {error}"))
                    .and_then(|context| {
                        reis::handshake::ei_handshake_blocking(
                            &context,
                            &name,
                            ei::handshake::ContextType::Sender,
                        )
                        .map(|response| (context, response))
                        .map_err(|error| format!("The input connection refused DeskLink: {error}"))
                    });
                match opened {
                    Ok((context, response)) => {
                        let converter = EiEventConverter::new(&context, response);
                        if let Ok(mut shared) = worker_shared.lock() {
                            shared.connected = true;
                        }
                        let _ = started.send(Ok(()));
                        run(context, converter, receiver, worker_shared);
                    }
                    Err(error) => {
                        let _ = started.send(Err(error));
                    }
                }
            })
            .map_err(|error| format!("Could not start the input worker: {error}"))?;
        handshake
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "The input connection did not answer".to_string())??;
        Ok(Self { commands, shared })
    }

    /// Waits until a resumed absolute region for `mapping_id` exists (of
    /// `width` logical pixels, when given: the compositor offers the region
    /// again after a scale change).
    pub(crate) fn wait_for_region(
        &self,
        mapping_id: Option<&str>,
        width: Option<u32>,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let ready = self.shared.lock().is_ok_and(|shared| {
                shared.regions.iter().any(|(id, resumed, region_width)| {
                    *resumed
                        && (mapping_id.is_none() || id.as_deref() == mapping_id)
                        && width.is_none_or(|width| region_width.abs_diff(width) <= 1)
                })
            });
            if ready {
                return true;
            }
            if !self.is_connected() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.shared.lock().is_ok_and(|shared| shared.connected)
    }

    pub(crate) fn send(&self, command: Command) -> Result<(), String> {
        if !self.is_connected() {
            return Err("The input connection closed".to_string());
        }
        self.commands
            .send(command)
            .map_err(|_| "The input connection closed".to_string())
    }
}

/// A device the server offered, once it says it is done.
struct Emulated {
    device: reis::event::Device,
    resumed: bool,
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_micros() as u64)
        .unwrap_or(0)
}

fn run(
    context: ei::Context,
    mut converter: EiEventConverter,
    commands: Receiver<Command>,
    shared: Arc<Mutex<Shared>>,
) {
    let mut devices: Vec<Emulated> = Vec::new();
    let mut sequence = 1u32;
    'serve: loop {
        // The server's events.
        let mut poll = libc::pollfd {
            fd: context.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut poll, 1, 4) };
        if ready > 0 {
            match context.read() {
                Ok(0) => break 'serve,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break 'serve,
            }
        }
        // Always drained: the handshake may already have read what follows
        // it, and then the socket never turns readable for it.
        {
            while let Some(pending) = context.pending_event() {
                match pending {
                    reis::PendingRequestResult::Request(request) => {
                        if let Err(error) = converter.handle_event(request) {
                            debug_log!("[DeskLink] EIS event refused: {error}");
                            break 'serve;
                        }
                    }
                    other => debug_log!(
                        "[DeskLink] EIS unreadable: {}",
                        match other {
                            reis::PendingRequestResult::ParseError(error) => format!("{error}"),
                            reis::PendingRequestResult::InvalidObject(id) => format!("object {id}"),
                            _ => String::new(),
                        }
                    ),
                }
            }
        }
        while let Some(event) = converter.next_event() {
            debug_log!("[DeskLink] EIS {}", describe(&event));
            match event {
                EiEvent::Disconnected(_) => break 'serve,
                EiEvent::SeatAdded(added) => added.seat.bind_capabilities(&[
                    DeviceCapability::Pointer,
                    DeviceCapability::PointerAbsolute,
                    DeviceCapability::Keyboard,
                    DeviceCapability::Scroll,
                    DeviceCapability::Button,
                ]),
                EiEvent::DeviceAdded(added) => devices.push(Emulated {
                    device: added.device,
                    resumed: false,
                }),
                EiEvent::DeviceRemoved(removed) => {
                    devices.retain(|known| known.device != removed.device)
                }
                EiEvent::DeviceResumed(resumed) => {
                    if let Some(known) = devices.iter_mut().find(|k| k.device == resumed.device) {
                        known.resumed = true;
                        known
                            .device
                            .device()
                            .start_emulating(resumed.serial, sequence);
                        sequence = sequence.wrapping_add(1);
                    }
                }
                EiEvent::DevicePaused(paused) => {
                    if let Some(known) = devices.iter_mut().find(|k| k.device == paused.device) {
                        known.resumed = false;
                    }
                }
                _ => {}
            }
        }
        if let Ok(mut shared) = shared.lock() {
            shared.regions = devices
                .iter()
                .filter(|known| {
                    known
                        .device
                        .has_capability(DeviceCapability::PointerAbsolute)
                })
                .flat_map(|known| {
                    known
                        .device
                        .regions()
                        .iter()
                        .map(|region| (region.mapping_id.clone(), known.resumed, region.width))
                        .collect::<Vec<_>>()
                })
                .collect();
        }
        // What the share asked for.
        loop {
            match commands.try_recv() {
                Ok(command) => {
                    let serial = converter.connection().serial();
                    send(&devices, &command, serial);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break 'serve,
            }
        }
        if context.flush().is_err() {
            break 'serve;
        }
    }
    if let Ok(mut shared) = shared.lock() {
        shared.connected = false;
        shared.regions.clear();
    }
}

/// One line for the debug log.
fn describe(event: &EiEvent) -> String {
    let device = |device: &reis::event::Device| {
        let regions = device
            .regions()
            .iter()
            .map(|r| {
                format!(
                    "{}x{}+{}+{} {:?}",
                    r.width, r.height, r.x, r.y, r.mapping_id
                )
            })
            .collect::<Vec<_>>();
        let caps = [
            (DeviceCapability::Pointer, "pointer"),
            (DeviceCapability::PointerAbsolute, "absolute"),
            (DeviceCapability::Keyboard, "keyboard"),
            (DeviceCapability::Scroll, "scroll"),
            (DeviceCapability::Button, "button"),
        ]
        .iter()
        .filter(|(cap, _)| device.has_capability(*cap))
        .map(|(_, name)| *name)
        .collect::<Vec<_>>();
        format!("{:?} {caps:?} regions {regions:?}", device.name())
    };
    match event {
        EiEvent::SeatAdded(added) => format!("seat {:?}", added.seat.name()),
        EiEvent::DeviceAdded(added) => format!("device added {}", device(&added.device)),
        EiEvent::DeviceRemoved(removed) => format!("device removed {}", device(&removed.device)),
        EiEvent::DeviceResumed(resumed) => format!("device resumed {}", device(&resumed.device)),
        EiEvent::DevicePaused(paused) => format!("device paused {}", device(&paused.device)),
        EiEvent::Disconnected(gone) => {
            format!("disconnected {:?} {}", gone.reason, gone.explanation)
        }
        other => format!("{other:?}").chars().take(80).collect(),
    }
}

/// The resumed device offering `capability`.
fn device_with(devices: &[Emulated], capability: DeviceCapability) -> Option<&Emulated> {
    devices
        .iter()
        .find(|known| known.resumed && known.device.has_capability(capability))
}

fn send(devices: &[Emulated], command: &Command, serial: u32) {
    let frame = |known: &Emulated| known.device.device().frame(serial, now_us());
    match command {
        Command::Absolute { mapping_id, x, y } => {
            let target = devices
                .iter()
                .filter(|known| {
                    known.resumed
                        && known
                            .device
                            .has_capability(DeviceCapability::PointerAbsolute)
                })
                .find_map(|known| {
                    known
                        .device
                        .regions()
                        .iter()
                        .find(|region| {
                            mapping_id.is_none()
                                || region.mapping_id.as_deref() == mapping_id.as_deref()
                        })
                        .map(|region| (known, region_point(region, *x, *y)))
                });
            if let Some((known, (px, py))) = target {
                if let Some(pointer) = known.device.interface::<ei::PointerAbsolute>() {
                    pointer.motion_absolute(px, py);
                    frame(known);
                }
            }
        }
        Command::Relative(dx, dy) => {
            if let Some(known) = device_with(devices, DeviceCapability::Pointer) {
                if let Some(pointer) = known.device.interface::<ei::Pointer>() {
                    pointer.motion_relative(*dx as f32, *dy as f32);
                    frame(known);
                }
            }
        }
        Command::Button(code, pressed) => {
            if let Some(known) = device_with(devices, DeviceCapability::Button) {
                if let Some(button) = known.device.interface::<ei::Button>() {
                    button.button(
                        *code,
                        if *pressed {
                            ei::button::ButtonState::Press
                        } else {
                            ei::button::ButtonState::Released
                        },
                    );
                    frame(known);
                }
            }
        }
        Command::Scroll { dx, dy, stop } => {
            if let Some(known) = device_with(devices, DeviceCapability::Scroll) {
                if let Some(scroll) = known.device.interface::<ei::Scroll>() {
                    if *stop {
                        scroll.scroll_stop(1, 1, 0);
                    } else {
                        scroll.scroll(*dx as f32, *dy as f32);
                    }
                    frame(known);
                }
            }
        }
        Command::Key(code, pressed) => {
            if let Some(known) = device_with(devices, DeviceCapability::Keyboard) {
                if let Some(keyboard) = known.device.interface::<ei::Keyboard>() {
                    keyboard.key(
                        *code,
                        if *pressed {
                            ei::keyboard::KeyState::Press
                        } else {
                            ei::keyboard::KeyState::Released
                        },
                    );
                    frame(known);
                }
            }
        }
    }
}

/// One event from a receiver context (input capture, F4 glide): what the
/// compositor captured, evdev codes and logical-pixel deltas.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Captured {
    /// The compositor started sending: `sequence` is the portal activation
    /// id this burst belongs to.
    Start(u32),
    Motion(f64, f64),
    Button(u32, bool),
    /// Smooth scroll, logical pixels.
    Scroll(f64, f64),
    /// Wheel clicks, in 120ths of a click.
    Discrete(i32, i32),
    Key(u32, bool),
    /// The locked modifier mask (XKB: Caps Lock is 2).
    Locked(u32),
    /// The connection closed.
    Closed,
}

/// How long the receiver's handshake may take before it is given up.
const RECEIVER_HANDSHAKE: Duration = Duration::from_secs(5);

/// Starts a receiver context on an input-capture portal's socket: a thread
/// that forwards what it captures to `forward` until the socket closes or
/// the other end hangs up. A full channel drops events (counted), never
/// blocks or grows.
pub(crate) fn spawn_receiver(
    socket: OwnedFd,
    name: &str,
    forward: impl Fn(Captured) -> bool + Send + 'static,
) -> Result<(), String> {
    let stream = UnixStream::from(socket);
    // The handshake reads blocking; a server that never answers must not
    // hold the thread forever.
    let _ = stream.set_read_timeout(Some(RECEIVER_HANDSHAKE));
    let name = name.to_string();
    std::thread::Builder::new()
        .name("desklink-ei-capture".into())
        .spawn(move || {
            let Ok(context) = ei::Context::new(stream) else {
                forward(Captured::Closed);
                return;
            };
            let response = match reis::handshake::ei_handshake_blocking(
                &context,
                &name,
                ei::handshake::ContextType::Receiver,
            ) {
                Ok(response) => response,
                Err(error) => {
                    eprintln!("[DeskLink] Input capture refused DeskLink: {error}");
                    forward(Captured::Closed);
                    return;
                }
            };
            let mut converter = EiEventConverter::new(&context, response);
            receive(&context, &mut converter, &forward);
            forward(Captured::Closed);
        })
        .map(|_| ())
        .map_err(|error| format!("Could not start the input capture reader: {error}"))
}

fn receive(
    context: &ei::Context,
    converter: &mut EiEventConverter,
    forward: &impl Fn(Captured) -> bool,
) {
    loop {
        let mut poll = libc::pollfd {
            fd: context.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut poll, 1, 100) };
        if ready > 0 {
            match context.read() {
                Ok(0) => return,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return,
            }
        }
        while let Some(pending) = context.pending_event() {
            if let reis::PendingRequestResult::Request(request) = pending {
                if converter.handle_event(request).is_err() {
                    return;
                }
            }
        }
        while let Some(event) = converter.next_event() {
            let captured = match event {
                EiEvent::Disconnected(_) => return,
                EiEvent::SeatAdded(added) => {
                    added.seat.bind_capabilities(&[
                        DeviceCapability::Pointer,
                        DeviceCapability::Button,
                        DeviceCapability::Scroll,
                        DeviceCapability::Keyboard,
                    ]);
                    None
                }
                EiEvent::DeviceStartEmulating(start) => Some(Captured::Start(start.sequence)),
                EiEvent::PointerMotion(motion) => {
                    Some(Captured::Motion(f64::from(motion.dx), f64::from(motion.dy)))
                }
                EiEvent::Button(button) => Some(Captured::Button(
                    button.button,
                    button.state == ei::button::ButtonState::Press,
                )),
                EiEvent::ScrollDelta(scroll) => {
                    Some(Captured::Scroll(f64::from(scroll.dx), f64::from(scroll.dy)))
                }
                EiEvent::ScrollDiscrete(scroll) => {
                    Some(Captured::Discrete(scroll.discrete_dx, scroll.discrete_dy))
                }
                EiEvent::KeyboardKey(key) => Some(Captured::Key(
                    key.key,
                    key.state == ei::keyboard::KeyState::Press,
                )),
                EiEvent::KeyboardModifiers(modifiers) => Some(Captured::Locked(modifiers.locked)),
                _ => None,
            };
            if let Some(captured) = captured {
                if !forward(captured) {
                    return;
                }
            }
        }
        if context.flush().is_err() {
            return;
        }
    }
}

/// A point at fractions `(x, y)` of `region`, inside it.
fn region_point(region: &reis::event::Region, x: f64, y: f64) -> (f32, f32) {
    let inside = |origin: u32, size: u32, fraction: f64| {
        let size = f64::from(size.max(1));
        (f64::from(origin) + (fraction.clamp(0.0, 1.0) * size).min(size - 1.0)) as f32
    };
    (
        inside(region.x, region.width, x),
        inside(region.y, region.height, y),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fraction_lands_inside_its_region() {
        let region = reis::event::Region {
            x: 1536,
            y: 0,
            width: 1200,
            height: 540,
            scale: 2.0,
            mapping_id: Some("m".into()),
        };
        assert_eq!(region_point(&region, 0.0, 0.0), (1536.0, 0.0));
        assert_eq!(region_point(&region, 0.5, 0.5), (2136.0, 270.0));
        assert_eq!(
            region_point(&region, 1.0, 1.0),
            (2735.0, 539.0),
            "never past the edge"
        );
        assert_eq!(region_point(&region, -3.0, 9.0), (1536.0, 539.0), "clamped");
    }
}
