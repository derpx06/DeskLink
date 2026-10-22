//! "Type on <phone>" (contract C2): this computer's keyboard typing into
//! whatever text field has focus on the phone. A small focused window takes
//! the keys (through an input method, so composed and non-Latin text works)
//! and sends each as `desklink.mousepad.request` with `target:
//! "focused-field"` — keys only, no pointer, no screen session. It shows
//! whether the phone reports a focused field (`mousepad.keyboardstate`).
//! Esc closes it.

use std::cell::RefCell;
use std::collections::HashMap;

use adw::prelude::*;
use gtk::gdk::{Key, ModifierType};
use gtk::glib;
use serde_json::{Map, Value};

use crate::device_links::daemon::{DaemonCommand, DaemonHandle};

pub(crate) const TARGET: &str = "focused-field";

/// The special keys the phone acts on (contract C2): Backspace, arrows,
/// Home/End, Enter, Delete. Esc closes the window instead; Tab and F-keys
/// are not sent (the phone ignores them).
fn special(key: Key) -> Option<i64> {
    Some(match key {
        Key::BackSpace => 1,
        Key::Left => 4,
        Key::Up => 5,
        Key::Right => 6,
        Key::Down => 7,
        Key::Home => 10,
        Key::End => 11,
        Key::Return | Key::KP_Enter => 12,
        Key::Delete | Key::KP_Delete => 13,
        _ => return None,
    })
}

fn base(state: ModifierType) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("target".into(), TARGET.into());
    for (name, mask) in [
        ("shift", ModifierType::SHIFT_MASK),
        ("ctrl", ModifierType::CONTROL_MASK),
        ("alt", ModifierType::ALT_MASK),
    ] {
        if state.contains(mask) {
            payload.insert(name.into(), true.into());
        }
    }
    payload
}

/// The packet body for a key press that is not text: a special key, or
/// Ctrl+A/C/X/V. `None` for anything else (text comes from the input method).
pub(crate) fn key_payload(key: Key, state: ModifierType) -> Option<Map<String, Value>> {
    let mut payload = base(state);
    if let Some(code) = special(key) {
        payload.insert("specialKey".into(), code.into());
        return Some(payload);
    }
    if state.contains(ModifierType::CONTROL_MASK) {
        let letter = key.to_lower().to_unicode()?;
        if matches!(letter, 'a' | 'c' | 'x' | 'v') {
            payload.insert("key".into(), letter.to_string().into());
            return Some(payload);
        }
    }
    None
}

/// The body for typed text (an input method commit).
pub(crate) fn text_payload(text: &str) -> Option<Map<String, Value>> {
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    if text.is_empty() {
        return None;
    }
    let mut payload = base(ModifierType::empty());
    payload.insert("key".into(), text.into());
    Some(payload)
}

/// Whether `device` can be typed on: it takes mouse-pad requests and reports
/// its focused field.
pub(crate) fn supported(device: &crate::device_links::device::DeviceView) -> bool {
    use crate::device_links::packet::{
        PACKET_TYPE_MOUSEPAD_KEYBOARDSTATE, PACKET_TYPE_MOUSEPAD_REQUEST,
    };
    device
        .incoming_capabilities
        .iter()
        .any(|capability| capability == PACKET_TYPE_MOUSEPAD_REQUEST)
        && device
            .outgoing_capabilities
            .iter()
            .any(|capability| capability == PACKET_TYPE_MOUSEPAD_KEYBOARDSTATE)
}

/// The line under the title: what the phone says about its focus, or that
/// this computer (`desk`) may not type there.
pub(crate) fn state_words(
    phone: &str,
    desk: &str,
    focused: Option<bool>,
    allowed: Option<bool>,
) -> String {
    if allowed == Some(false) {
        return format!(
            "Typing is off on {phone} — tap Allow in the notification on the phone \
             (or Settings → {desk} → Let {desk} type on this phone)"
        );
    }
    match focused {
        Some(true) => format!("Typing into the field in focus on {phone}"),
        Some(false) => format!("Tap a text field on {phone} to type there"),
        // Nothing reported on this connection yet.
        None => format!("Waiting for {phone}…"),
    }
}

/// This computer's name as the phone shows it (its host name).
fn desk_name() -> String {
    hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this computer".to_string())
}

thread_local! {
    /// One window per phone.
    static OPEN: RefCell<HashMap<String, glib::WeakRef<adw::Window>>> = RefCell::default();
}

/// Opens (or raises) the "Type on <phone>" window.
pub(crate) fn open(daemon: &DaemonHandle, device_id: &str, parent: Option<&gtk::Window>) {
    if let Some(window) = OPEN.with(|open| open.borrow().get(device_id).and_then(|w| w.upgrade())) {
        window.present();
        return;
    }
    let name = daemon
        .devices()
        .into_iter()
        .find(|device| device.id == device_id)
        .map(|device| device.name)
        .unwrap_or_else(|| "the phone".into());
    let window = adw::Window::builder()
        .title(format!("Type on {name}"))
        .default_width(420)
        .default_height(280)
        .modal(false)
        .build();
    if let Some(parent) = parent {
        window.set_transient_for(Some(parent));
    }
    let icon = gtk::Image::builder()
        .icon_name("input-keyboard-symbolic")
        .pixel_size(48)
        .css_classes(["dim-label"])
        .build();
    let title = gtk::Label::builder()
        .label("Your keys go to the phone")
        .css_classes(["title-3"])
        .build();
    let status = gtk::Label::builder()
        .label(state_words(&name, "", None, None))
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    let hint = gtk::Label::builder()
        .label("Esc to stop")
        .css_classes(["dim-label", "caption"])
        .build();
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .valign(gtk::Align::Center)
        .vexpand(true)
        .margin_start(24)
        .margin_end(24)
        .margin_bottom(18)
        .build();
    body.append(&icon);
    body.append(&title);
    body.append(&status);
    body.append(&hint);
    // The surface that has focus and takes every key.
    let surface = gtk::Box::builder().focusable(true).can_focus(true).build();
    surface.append(&body);
    surface.update_property(&[gtk::accessible::Property::Label(&format!(
        "Type on {name}: your keys go to the phone. Esc to stop."
    ))]);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&surface));
    window.set_content(Some(&toolbar));

    let send = {
        let (daemon, id) = (daemon.clone(), device_id.to_string());
        move |payload: Map<String, Value>| {
            daemon.send(DaemonCommand::SendFocusedFieldKeys(id.clone(), payload))
        }
    };
    let im = gtk::IMMulticontext::new();
    let keys = gtk::EventControllerKey::new();
    keys.set_im_context(Some(&im));
    {
        let send = send.clone();
        im.connect_commit(move |_, text| {
            if let Some(payload) = text_payload(text) {
                send(payload);
            }
        });
    }
    {
        let window = window.downgrade();
        keys.connect_key_pressed(move |_, key, _, state| {
            if key == Key::Escape {
                if let Some(window) = window.upgrade() {
                    window.close();
                }
                return glib::Propagation::Stop;
            }
            match key_payload(key, state) {
                Some(payload) => {
                    send(payload);
                    glib::Propagation::Stop
                }
                None => glib::Propagation::Proceed,
            }
        });
    }
    surface.add_controller(keys);
    {
        let im = im.clone();
        let focus = gtk::EventControllerFocus::new();
        let out = im.clone();
        focus.connect_enter(move |_| im.focus_in());
        focus.connect_leave(move |_| out.focus_out());
        surface.add_controller(focus);
    }

    // The phone's focus state, as it reports it.
    {
        let (daemon, id, status) = (daemon.clone(), device_id.to_string(), status.downgrade());
        let name = name.clone();
        let desk = desk_name();
        glib::timeout_add_local(std::time::Duration::from_millis(300), move || {
            let Some(status) = status.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let device = daemon.devices().into_iter().find(|device| device.id == id);
            let words = match device.as_ref() {
                Some(device) if device.feature_transport_ready => state_words(
                    &name,
                    &desk,
                    device.keyboard_focused,
                    device.keyboard_allowed,
                ),
                _ => format!("{name} is not connected"),
            };
            if status.label() != words {
                status.set_label(&words);
            }
            glib::ControlFlow::Continue
        });
    }
    OPEN.with(|open| {
        open.borrow_mut()
            .insert(device_id.to_string(), window.downgrade())
    });
    window.present();
    surface.grab_focus();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_become_focused_field_packets() {
        let enter = key_payload(Key::Return, ModifierType::empty()).unwrap();
        assert_eq!(enter.get("target"), Some(&Value::from("focused-field")));
        assert_eq!(enter.get("specialKey"), Some(&Value::from(12)));
        let back = key_payload(Key::BackSpace, ModifierType::CONTROL_MASK).unwrap();
        assert_eq!(back.get("specialKey"), Some(&Value::from(1)));
        assert_eq!(back.get("ctrl"), Some(&Value::from(true)));
        let paste = key_payload(Key::v, ModifierType::CONTROL_MASK).unwrap();
        assert_eq!(paste.get("key"), Some(&Value::from("v")));
        assert_eq!(paste.get("ctrl"), Some(&Value::from(true)));
        // Plain letters come from the input method, Tab and F-keys never.
        assert!(key_payload(Key::a, ModifierType::empty()).is_none());
        assert!(key_payload(Key::Tab, ModifierType::empty()).is_none());
        assert!(key_payload(Key::F5, ModifierType::empty()).is_none());
        assert!(key_payload(Key::z, ModifierType::CONTROL_MASK).is_none());

        let text = text_payload("héllo 你好").unwrap();
        assert_eq!(text.get("key"), Some(&Value::from("héllo 你好")));
        assert!(text.get("specialKey").is_none() && text.get("dx").is_none());
        assert!(text_payload("\u{7}").is_none());
    }

    #[test]
    fn the_window_says_what_the_phone_reports() {
        assert_eq!(
            state_words("Pixel", "desk", Some(true), None),
            "Typing into the field in focus on Pixel"
        );
        assert_eq!(
            state_words("Pixel", "desk", Some(false), None),
            "Tap a text field on Pixel to type there"
        );
        assert_eq!(
            state_words("Pixel", "desk", None, None),
            "Waiting for Pixel…",
            "no report yet: nothing implies a field is needed"
        );
        // The phone refused this computer: that, whatever the focus.
        for focused in [None, Some(false), Some(true)] {
            assert_eq!(
                state_words("Pixel", "desk", focused, Some(false)),
                "Typing is off on Pixel — tap Allow in the notification on the phone \
                 (or Settings → desk → Let desk type on this phone)"
            );
        }
    }
}
