//! Remote input backends for GNOME Wayland and X11.
//!
//! Wayland input is authorized through the XDG RemoteDesktop portal. The
//! portal setup waits for every request response before the backend becomes
//! usable, so an incoming packet cannot inject input before the user grants
//! permission. X11 remains available only when the active session is X11.

use ashpd::desktop::{
    remote_desktop::{DeviceType, KeyState, RemoteDesktop},
    PersistMode,
};
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse};
use futures::executor::block_on;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread;
use std::time::Duration;

enum InputCommand {
    MoveRelative(i32, i32, SyncSender<Result<(), String>>),
    Scroll(i32, Axis, SyncSender<Result<(), String>>),
    Button(Button, Direction, SyncSender<Result<(), String>>),
    Key(Key, Direction, SyncSender<Result<(), String>>),
    Text(String, SyncSender<Result<(), String>>),
    Close,
}

pub(crate) struct PortalInput {
    sender: Sender<InputCommand>,
    worker: Option<thread::JoinHandle<()>>,
}

impl std::fmt::Debug for PortalInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PortalInput")
    }
}

impl PortalInput {
    fn new() -> Result<Self, String> {
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("desklink-remote-desktop-portal".to_string())
            .spawn(move || portal_worker(receiver, ready_sender))
            .map_err(|error| format!("Could not start RemoteDesktop portal worker: {error}"))?;

        match ready_receiver.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(())) => Ok(Self {
                sender,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(error) => Err(format!(
                "RemoteDesktop portal permission request did not complete: {error}"
            )),
        }
    }

    fn send(
        &self,
        command: impl FnOnce(SyncSender<Result<(), String>>) -> InputCommand,
    ) -> Result<(), String> {
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        self.sender
            .send(command(reply_sender))
            .map_err(|_| "RemoteDesktop portal session is closed".to_string())?;
        reply_receiver
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "RemoteDesktop portal action timed out".to_string())?
    }
}

impl Drop for PortalInput {
    fn drop(&mut self) {
        let _ = self.sender.send(InputCommand::Close);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Input backend selected from the actual desktop session type.
pub(crate) enum RemoteInputBackend {
    Portal {
        input: PortalInput,
        last_absolute: Option<(i32, i32)>,
    },
    X11(Box<Enigo>),
}

impl std::fmt::Debug for RemoteInputBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Portal { .. } => formatter.write_str("RemoteInputBackend::Portal"),
            Self::X11(_) => formatter.write_str("RemoteInputBackend::X11"),
        }
    }
}

impl RemoteInputBackend {
    pub fn new() -> Result<Self, String> {
        match std::env::var("XDG_SESSION_TYPE").ok().as_deref() {
            Some("wayland") => PortalInput::new().map(|input| Self::Portal {
                input,
                last_absolute: None,
            }),
            Some("x11") => Enigo::new(&settings())
                .map(|enigo| Self::X11(Box::new(enigo)))
                .map_err(|error| format!("X11 input backend unavailable: {error}")),
            Some(other) => Err(format!("Unsupported desktop session type: {other}")),
            None => Err("Desktop session type is unavailable; refusing remote input".to_string()),
        }
    }

    pub fn move_mouse(&mut self, x: i32, y: i32, coordinate: Coordinate) -> Result<(), String> {
        match self {
            Self::X11(enigo) => enigo
                .move_mouse(x, y, coordinate)
                .map_err(|error| error.to_string()),
            Self::Portal {
                input,
                last_absolute,
            } => match coordinate {
                Coordinate::Rel => input.send(|reply| InputCommand::MoveRelative(x, y, reply)),
                Coordinate::Abs => {
                    let previous = last_absolute.replace((x, y));
                    match previous {
                        Some((previous_x, previous_y)) => input.send(|reply| {
                            InputCommand::MoveRelative(
                                x.saturating_sub(previous_x),
                                y.saturating_sub(previous_y),
                                reply,
                            )
                        }),
                        None => Ok(()),
                    }
                }
            },
        }
    }

    pub fn scroll(&mut self, amount: i32, axis: Axis) -> Result<(), String> {
        match self {
            Self::X11(enigo) => enigo
                .scroll(amount, axis)
                .map_err(|error| error.to_string()),
            Self::Portal { input, .. } => {
                input.send(|reply| InputCommand::Scroll(amount, axis, reply))
            }
        }
    }

    pub fn button(&mut self, button: Button, direction: Direction) -> Result<(), String> {
        match self {
            Self::X11(enigo) => enigo
                .button(button, direction)
                .map_err(|error| error.to_string()),
            Self::Portal { input, .. } => {
                input.send(|reply| InputCommand::Button(button, direction, reply))
            }
        }
    }

    pub fn key(&mut self, key: Key, direction: Direction) -> Result<(), String> {
        match self {
            Self::X11(enigo) => enigo.key(key, direction).map_err(|error| error.to_string()),
            Self::Portal { input, .. } => {
                input.send(|reply| InputCommand::Key(key, direction, reply))
            }
        }
    }

    pub fn text(&mut self, text: &str) -> Result<(), String> {
        match self {
            Self::X11(enigo) => enigo.text(text).map_err(|error| error.to_string()),
            Self::Portal { input, .. } => {
                input.send(|reply| InputCommand::Text(text.to_string(), reply))
            }
        }
    }
}

fn portal_worker(receiver: Receiver<InputCommand>, ready: Sender<Result<(), String>>) {
    let remote_desktop = match block_on(RemoteDesktop::new()) {
        Ok(proxy) => proxy,
        Err(error) => {
            let _ = ready.send(Err(format!("RemoteDesktop portal unavailable: {error}")));
            return;
        }
    };
    let session = match block_on(remote_desktop.create_session()) {
        Ok(session) => session,
        Err(error) => {
            let _ = ready.send(Err(format!(
                "Could not create RemoteDesktop session: {error}"
            )));
            return;
        }
    };
    let selection = match block_on(remote_desktop.select_devices(
        &session,
        DeviceType::Keyboard | DeviceType::Pointer,
        None,
        // Keep the RemoteDesktop grant until the user revokes it. The
        // packet reader owns one backend per connection and will not retry a
        // denied request for every incoming input packet.
        PersistMode::ExplicitlyRevoked,
    )) {
        Ok(request) => request,
        Err(error) => {
            let _ = ready.send(Err(format!(
                "Could not request remote input devices: {error}"
            )));
            return;
        }
    };
    if let Err(error) = selection.response() {
        let _ = ready.send(Err(format!("Remote input permission was denied: {error}")));
        return;
    }
    let start = match block_on(remote_desktop.start(&session, None)) {
        Ok(request) => request,
        Err(error) => {
            let _ = ready.send(Err(format!(
                "Could not start RemoteDesktop session: {error}"
            )));
            return;
        }
    };
    if let Err(error) = start.response() {
        let _ = ready.send(Err(format!("Remote input permission was denied: {error}")));
        return;
    }
    let _ = ready.send(Ok(()));

    while let Ok(command) = receiver.recv() {
        match command {
            InputCommand::Close => {
                let _ = block_on(session.close());
                break;
            }
            InputCommand::MoveRelative(dx, dy, reply) => {
                let result =
                    block_on(remote_desktop.notify_pointer_motion(&session, dx as f64, dy as f64))
                        .map_err(|error| format!("Portal pointer motion failed: {error}"));
                let _ = reply.send(result);
            }
            InputCommand::Scroll(amount, axis, reply) => {
                let (dx, dy) = match axis {
                    Axis::Horizontal => (amount as f64, 0.0),
                    Axis::Vertical => (0.0, amount as f64),
                };
                let result = block_on(remote_desktop.notify_pointer_axis(&session, dx, dy, true))
                    .map_err(|error| format!("Portal scroll failed: {error}"));
                let _ = reply.send(result);
            }
            InputCommand::Button(button, direction, reply) => {
                let result = match portal_button(button) {
                    Ok(button) => {
                        block_on(notify_button(&remote_desktop, &session, button, direction))
                            .map_err(|error| format!("Portal pointer button failed: {error}"))
                    }
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            InputCommand::Key(key, direction, reply) => {
                let result = match portal_keysym(key) {
                    Ok(keysym) => {
                        block_on(notify_key(&remote_desktop, &session, keysym, direction))
                            .map_err(|error| format!("Portal keyboard input failed: {error}"))
                    }
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            InputCommand::Text(text, reply) => {
                let result = type_portal_text(&remote_desktop, &session, &text);
                let _ = reply.send(result);
            }
        }
    }
    let _ = block_on(session.close());
}

async fn notify_button(
    remote_desktop: &RemoteDesktop<'_>,
    session: &ashpd::desktop::Session<'_, RemoteDesktop<'_>>,
    button: i32,
    direction: Direction,
) -> Result<(), ashpd::Error> {
    match direction {
        Direction::Click => {
            remote_desktop
                .notify_pointer_button(session, button, KeyState::Pressed)
                .await?;
            remote_desktop
                .notify_pointer_button(session, button, KeyState::Released)
                .await
        }
        Direction::Press => {
            remote_desktop
                .notify_pointer_button(session, button, KeyState::Pressed)
                .await
        }
        Direction::Release => {
            remote_desktop
                .notify_pointer_button(session, button, KeyState::Released)
                .await
        }
    }
}

async fn notify_key(
    remote_desktop: &RemoteDesktop<'_>,
    session: &ashpd::desktop::Session<'_, RemoteDesktop<'_>>,
    keysym: i32,
    direction: Direction,
) -> Result<(), ashpd::Error> {
    match direction {
        Direction::Click => {
            remote_desktop
                .notify_keyboard_keysym(session, keysym, KeyState::Pressed)
                .await?;
            remote_desktop
                .notify_keyboard_keysym(session, keysym, KeyState::Released)
                .await
        }
        Direction::Press => {
            remote_desktop
                .notify_keyboard_keysym(session, keysym, KeyState::Pressed)
                .await
        }
        Direction::Release => {
            remote_desktop
                .notify_keyboard_keysym(session, keysym, KeyState::Released)
                .await
        }
    }
}

fn type_portal_text(
    remote_desktop: &RemoteDesktop<'_>,
    session: &ashpd::desktop::Session<'_, RemoteDesktop<'_>>,
    text: &str,
) -> Result<(), String> {
    for character in text.chars() {
        if !character.is_ascii() {
            return Err("Portal text input currently supports ASCII keysyms only".to_string());
        }
        block_on(notify_key(
            remote_desktop,
            session,
            character as i32,
            Direction::Click,
        ))
        .map_err(|error| format!("Portal text input failed: {error}"))?;
    }
    Ok(())
}

fn portal_button(button: Button) -> Result<i32, String> {
    match button {
        Button::Left => Ok(0x110),
        Button::Right => Ok(0x111),
        Button::Middle => Ok(0x112),
        _ => Err("The portal backend does not support this pointer button".to_string()),
    }
}

fn portal_keysym(key: Key) -> Result<i32, String> {
    let keysym = match key {
        Key::Unicode(character) if character.is_ascii() => character as i32,
        Key::Backspace => 0xff08,
        Key::Tab => 0xff09,
        Key::Return => 0xff0d,
        Key::Escape => 0xff1b,
        Key::Delete => 0xffff,
        Key::Home => 0xff50,
        Key::End => 0xff57,
        Key::PageUp => 0xff55,
        Key::PageDown => 0xff56,
        Key::LeftArrow => 0xff51,
        Key::UpArrow => 0xff52,
        Key::RightArrow => 0xff53,
        Key::DownArrow => 0xff54,
        Key::Shift => 0xffe1,
        Key::Control => 0xffe3,
        Key::Alt => 0xffe9,
        Key::Meta => 0xffeb,
        Key::F1 => 0xffbe,
        Key::F2 => 0xffbf,
        Key::F3 => 0xffc0,
        Key::F4 => 0xffc1,
        Key::F5 => 0xffc2,
        Key::F6 => 0xffc3,
        Key::F7 => 0xffc4,
        Key::F8 => 0xffc5,
        Key::F9 => 0xffc6,
        Key::F10 => 0xffc7,
        Key::F11 => 0xffc8,
        Key::F12 => 0xffc9,
        _ => return Err(format!("The portal backend cannot map key {key:?}")),
    };
    Ok(keysym)
}

/// Settings used by the X11 fallback. A Wayland session never reaches this
/// path, even if XWayland is present.
pub fn settings() -> enigo::Settings {
    let mut settings = enigo::Settings::default();
    if std::env::var("XDG_SESSION_TYPE").ok().as_deref() == Some("wayland") {
        settings.x11_display = Some("desklink-wayland-no-x11".to_string());
    }
    settings
}

#[cfg(test)]
mod tests {
    use super::{portal_button, portal_keysym};
    use enigo::{Button, Key};

    #[test]
    fn portal_button_uses_linux_evdev_codes() {
        assert_eq!(portal_button(Button::Left).unwrap(), 0x110);
        assert_eq!(portal_button(Button::Right).unwrap(), 0x111);
    }

    #[test]
    fn portal_key_mapping_is_explicit() {
        assert_eq!(portal_keysym(Key::Return).unwrap(), 0xff0d);
        assert_eq!(portal_keysym(Key::Unicode('a')).unwrap(), 'a' as i32);
        assert!(portal_keysym(Key::Unicode('λ')).is_err());
    }
}
