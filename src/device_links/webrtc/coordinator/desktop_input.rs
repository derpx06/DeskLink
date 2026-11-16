//! The phone's input on this computer's screen: authorising it, the rate limit,
//! releasing held keys and buttons, the input log, and dispatch to the share's
//! portal worker.
//!
//! Sibling edges: `desktop_share` (the service registry and its commands).
//! Outside: `remote_desktop_portal`, `remote_session`, `limits`.

use super::*;

pub(super) fn release_held_portal_inputs(
    portal: &dyn DesktopSession,
    held_inputs: &mut std::collections::HashSet<(bool, i32)>,
) {
    for (is_key, code) in held_inputs.drain() {
        let action = if is_key {
            PortalInputAction::Key {
                keysym: code,
                pressed: false,
            }
        } else {
            PortalInputAction::Button {
                evdev_code: code,
                pressed: false,
            }
        };
        let _ = portal.apply(action);
    }
}

pub(crate) fn dispatch_desktop_remote_input(
    binding: &SessionBinding,
    packet: &NetworkPacket,
) -> Result<(), String> {
    let sender = desktop_portal_services()
        .lock()
        .map_err(|_| "Desktop portal service registry lock poisoned".to_string())?
        .get(&binding.device_id)
        .map(|service| service.sender.clone())
        .ok_or_else(|| "No approved desktop-control portal session is active".to_string())?;
    let actions = {
        let mut sessions = remote_session::sessions()
            .lock()
            .map_err(|_| "DeskLink remote-session lock poisoned".to_string())?;
        let session = sessions
            .get_mut(&binding.device_id)
            .ok_or_else(|| "DeskLink remote session has not started".to_string())?;
        authorize_desktop_remote_input(session, packet, &binding.device_id, now_millis())?
    };
    // Not past what a person (or glide, 500 a second) can produce, and never
    // a backlog: while the portal is slow or wedged, input waiting seconds
    // would be replayed as stale clicks and keys when it came back.
    if !PORTAL_INPUT_RATE.allow(&binding.device_id) {
        crate::device_links::limits::log_limited(
            "portal-input-rate",
            "Remote input arrived faster than it can be applied; some was dropped",
        );
        return Ok(());
    }
    match sender.try_send(DesktopPortalCommand::Input(actions)) {
        Ok(()) => Ok(()),
        Err(mpsc::TrySendError::Full(_)) => {
            crate::device_links::limits::log_limited(
                "portal-input-full",
                "The desktop is not taking remote input; some was dropped",
            );
            Ok(())
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {
            Err("Desktop portal worker has stopped".to_string())
        }
    }
}

/// Input packets accepted per second from one phone for the desktop share.
pub(super) const PORTAL_INPUT_PER_SECOND: u32 = 1_000;
pub(super) static PORTAL_INPUT_RATE: crate::device_links::limits::Buckets =
    crate::device_links::limits::Buckets::new(
        PORTAL_INPUT_PER_SECOND,
        std::time::Duration::from_secs(1),
    );

/// Debug builds: feeds a mousepad body into the active share as if the phone
/// had sent it, skipping the lease, so the GNOME side can be measured alone.
#[cfg(debug_assertions)]
pub(crate) fn debug_inject_desktop_input(device_id: &str, body: &str) -> Result<(), String> {
    let command = if body == "switch" {
        DesktopPortalCommand::SwitchScreen
    } else {
        let mut packet = NetworkPacket::new("desklink.mousepad.request");
        packet.body = serde_json::from_str(body).map_err(|error| error.to_string())?;
        DesktopPortalCommand::Input(input_actions(&packet)?)
    };
    desktop_portal_services()
        .lock()
        .map_err(|_| "registry lock poisoned".to_string())?
        .get(device_id)
        .ok_or("no active share")?
        .sender
        .try_send(command)
        .map_err(|_| "share worker stopped or busy".to_string())
}

pub(super) fn authorize_desktop_remote_input(
    session: &mut remote_session::RemoteSession,
    packet: &NetworkPacket,
    owner_device_id: &str,
    now: i64,
) -> Result<Vec<PortalInputAction>, String> {
    // A syntactically invalid input must not refresh the lease. Parse the
    // complete action batch first, then consume its sequence and extend expiry.
    let actions = input_actions(packet)?;
    session.verify_peer_input(packet, owner_device_id, now)?;
    #[cfg(debug_assertions)]
    debug_log!("[DeskLink] Phone input -> {}", input_log(&actions));
    Ok(actions)
}

/// Phone input as the log shows it: what kind, never which keys or text.
pub(super) fn input_log(actions: &[PortalInputAction]) -> String {
    let names: Vec<String> = actions
        .iter()
        .map(|action| match action {
            PortalInputAction::TypeText { text, .. } => {
                format!("TypeText({} chars)", text.chars().count())
            }
            PortalInputAction::Key { .. } => "Key".to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    names.join(", ")
}
