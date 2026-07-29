use adw::prelude::*;
use gtk::gdk;
use gtk::gdk::Key;
use gtk::glib;
use serde_json::{Map, Value};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use crate::device_links::daemon::{DaemonCommand, DaemonHandle};

pub fn show_remote_control_dialog(
    daemon: DaemonHandle,
    device_id: String,
    parent: &impl IsA<gtk::Window>,
) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        "
        .trackpad-box {
            background-color: alpha(@theme_bg_color, 0.4);
            border: 1px solid alpha(@theme_fg_color, 0.12);
            border-radius: 12px;
            margin: 20px;
            transition: border-color 0.25s ease, background-color 0.25s ease;
        }
        .trackpad-box:focus {
            background-color: alpha(@theme_selected_bg_color, 0.06);
            border: 2px solid @theme_selected_bg_color;
        }
        .status-pill {
            background-color: alpha(@theme_fg_color, 0.06);
            border-radius: 12px;
            padding: 6px 14px;
            font-size: 0.82rem;
            font-weight: bold;
            color: alpha(@theme_fg_color, 0.7);
        }
        .status-pill.active {
            background-color: alpha(@theme_selected_bg_color, 0.15);
            color: @theme_selected_bg_color;
        }
        .remote-entry {
            border-radius: 10px;
            padding: 8px 12px;
        }
    ",
    );
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(800)
        .default_height(600)
        .title("Remote Control")
        .build();

    let content_vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let header = adw::HeaderBar::new();
    content_vbox.append(&header);

    let trackpad_area = gtk::Box::builder()
        .hexpand(true)
        .vexpand(true)
        .css_classes(["trackpad-box"])
        .focusable(true)
        .build();

    let trackpad_vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();

    let status_label = gtk::Label::builder()
        .label("Phone screen requested")
        .css_classes(["status-pill"])
        .halign(gtk::Align::Center)
        .build();

    let screen_picture = gtk::Picture::new();
    screen_picture.set_hexpand(true);
    screen_picture.set_vexpand(true);
    screen_picture.set_can_shrink(true);
    screen_picture.set_size_request(320, 180);
    screen_picture.set_tooltip_text(Some("Live phone screen preview"));

    let trackpad_icon = gtk::Image::builder()
        .icon_name("input-touchpad-symbolic")
        .pixel_size(96)
        .halign(gtk::Align::Center)
        .opacity(0.5)
        .build();

    let label = gtk::Label::builder()
        .label("<b>Phone Screen Preview</b>\n\nWaiting for live frames from the phone\nPointer input is mapped to the phone screen")
        .use_markup(true)
        .justify(gtk::Justification::Center)
        .halign(gtk::Align::Center)
        .build();

    trackpad_vbox.append(&screen_picture);
    trackpad_vbox.append(&status_label);
    trackpad_vbox.append(&trackpad_icon);
    trackpad_vbox.append(&label);

    trackpad_area.append(&trackpad_vbox);
    content_vbox.append(&trackpad_area);

    let device_id_rc = Rc::new(device_id);
    daemon.send(DaemonCommand::SendScreenRequest(
        device_id_rc.to_string(),
        "phone-screen".to_string(),
    ));

    let daemon_clone = daemon.clone();
    let id_clone = device_id_rc.clone();
    window.connect_close_request(move |_| {
        daemon_clone.send(DaemonCommand::SendScreenStop(id_clone.to_string()));
        glib::Propagation::Proceed
    });

    // DeviceView owns the last authenticated frame.  Polling only this small
    // preview keeps the existing daemon/UI boundary intact while avoiding a
    // second screen decoder or a duplicate network session in the UI.
    let screen_picture_weak = screen_picture.downgrade();
    let status_weak = status_label.downgrade();
    let window_weak = window.downgrade();
    let daemon_frames = daemon.clone();
    let frame_device_id = device_id_rc.clone();
    let last_sequence = Rc::new(Cell::new(None::<u64>));
    let last_sequence_clone = last_sequence.clone();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if window_weak.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        let Some(device) = daemon_frames
            .devices()
            .into_iter()
            .find(|device| device.id == *frame_device_id)
        else {
            return glib::ControlFlow::Continue;
        };
        let Some(frame) = device.screen_frame else {
            return glib::ControlFlow::Continue;
        };
        if last_sequence_clone.get() == Some(frame.sequence) {
            return glib::ControlFlow::Continue;
        }
        let bytes = glib::Bytes::from(&frame.png);
        if let Ok(texture) = gdk::Texture::from_bytes(&bytes) {
            if let Some(picture) = screen_picture_weak.upgrade() {
                picture.set_paintable(Some(&texture));
            }
            if let Some(status) = status_weak.upgrade() {
                status.set_label("Live phone screen");
                status.add_css_class("active");
            }
            last_sequence_clone.set(Some(frame.sequence));
        }
        glib::ControlFlow::Continue
    });

    // 1. Mouse Movement (EventControllerMotion)
    let motion = gtk::EventControllerMotion::new();
    let daemon_clone = daemon.clone();
    let id_clone = device_id_rc.clone();

    let last_x = Rc::new(Cell::new(0.0));
    let last_y = Rc::new(Cell::new(0.0));

    let lx = last_x.clone();
    let ly = last_y.clone();
    motion.connect_enter(move |_, x, y| {
        lx.set(x);
        ly.set(y);
    });

    let lx = last_x.clone();
    let ly = last_y.clone();
    let trackpad_weak = trackpad_area.downgrade();
    motion.connect_motion(move |_, x, y| {
        let delta_x = x - lx.get();
        let delta_y = y - ly.get();
        lx.set(x);
        ly.set(y);

        if delta_x.abs() > 0.0001 || delta_y.abs() > 0.0001 {
            let mut payload = Map::new();
            if let Some(trackpad) = trackpad_weak.upgrade() {
                if let Some((remote_x, remote_y)) = map_preview_position(
                    x,
                    y,
                    trackpad.width(),
                    trackpad.height(),
                    DEFAULT_REMOTE_PHONE_WIDTH,
                    DEFAULT_REMOTE_PHONE_HEIGHT,
                ) {
                    payload.insert("x".to_string(), Value::from(remote_x));
                    payload.insert("y".to_string(), Value::from(remote_y));
                } else {
                    payload.insert("dx".to_string(), Value::from(delta_x));
                    payload.insert("dy".to_string(), Value::from(delta_y));
                }
            }
            daemon_clone.send(DaemonCommand::SendMousepadRequest(
                id_clone.to_string(),
                payload,
            ));
        }
    });
    trackpad_area.add_controller(motion);

    // 2. Mouse Clicks (GestureClick)
    let click = gtk::GestureClick::new();
    click.set_button(0); // Listen to all buttons
    let daemon_clone = daemon.clone();
    let id_clone = device_id_rc.clone();
    let trackpad_weak = trackpad_area.downgrade();
    click.connect_pressed(move |click, _, x, y| {
        if let Some(trackpad) = trackpad_weak.upgrade() {
            trackpad.grab_focus();
            let button = click.current_button();
            if let Some(mut payload) = click_payload(button) {
                if let Some((remote_x, remote_y)) = map_preview_position(
                    x,
                    y,
                    trackpad.width(),
                    trackpad.height(),
                    DEFAULT_REMOTE_PHONE_WIDTH,
                    DEFAULT_REMOTE_PHONE_HEIGHT,
                ) {
                    payload.insert("x".to_string(), Value::from(remote_x));
                    payload.insert("y".to_string(), Value::from(remote_y));
                }
                daemon_clone.send(DaemonCommand::SendMousepadRequest(
                    id_clone.to_string(),
                    payload,
                ));
            }
        }
    });

    trackpad_area.add_controller(click);

    // 3. Scroll (EventControllerScroll)
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let daemon_clone = daemon.clone();
    let id_clone = device_id_rc.clone();
    scroll.connect_scroll(move |_, dx, dy| {
        let mut payload = Map::new();
        payload.insert("scroll".to_string(), Value::Bool(true));
        payload.insert("dx".to_string(), Value::from(-dx * 3.0));
        payload.insert("dy".to_string(), Value::from(-dy * 3.0));
        daemon_clone.send(DaemonCommand::SendMousepadRequest(
            id_clone.to_string(),
            payload,
        ));
        glib::Propagation::Proceed
    });
    trackpad_area.add_controller(scroll);

    // 4. Keyboard Input (EventControllerKey)
    let key_controller = gtk::EventControllerKey::new();
    let daemon_clone = daemon.clone();
    let id_clone = device_id_rc.clone();
    let trackpad_weak = trackpad_area.downgrade();
    key_controller.connect_key_pressed(move |_, keyval, _keycode, state| {
        if let Some(trackpad) = trackpad_weak.upgrade() {
            if !trackpad.is_focus() {
                return glib::Propagation::Proceed;
            }
        }

        let mut payload = Map::new();
        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        let alt = state.contains(gdk::ModifierType::ALT_MASK);
        let super_mod = state.contains(gdk::ModifierType::SUPER_MASK);

        payload.insert("shift".to_string(), Value::Bool(shift));
        payload.insert("ctrl".to_string(), Value::Bool(ctrl));
        payload.insert("alt".to_string(), Value::Bool(alt));
        payload.insert("super".to_string(), Value::Bool(super_mod));

        let special_key = map_gdk_key_to_special(keyval);
        if special_key > 0 {
            payload.insert("specialKey".to_string(), Value::from(special_key));
        } else if let Some(text) = keyval.to_unicode() {
            if !text.is_control() {
                payload.insert("key".to_string(), Value::String(text.to_string()));
            }
        }

        if payload.contains_key("specialKey") || payload.contains_key("key") {
            daemon_clone.send(DaemonCommand::SendMousepadRequest(
                id_clone.to_string(),
                payload,
            ));
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_controller);

    // Focus tracking for trackpad_area
    let focus_controller = gtk::EventControllerFocus::new();
    let status_weak = status_label.downgrade();
    focus_controller.connect_enter(move |_| {
        if let Some(status) = status_weak.upgrade() {
            status.set_label("Keyboard input active");
            status.add_css_class("active");
        }
    });

    let status_weak2 = status_label.downgrade();
    focus_controller.connect_leave(move |_| {
        if let Some(status) = status_weak2.upgrade() {
            status.set_label("Click preview to type");
            status.remove_css_class("active");
        }
    });
    trackpad_area.add_controller(focus_controller);

    // 5. Beautiful text entry HBox with a send button
    let bottom_hbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_start(20)
        .margin_end(20)
        .margin_bottom(20)
        .build();

    let entry = gtk::Entry::builder()
        .placeholder_text("Type text here and press Enter to send...")
        .hexpand(true)
        .css_classes(["remote-entry"])
        .primary_icon_name("input-keyboard-symbolic")
        .build();

    let send_btn = gtk::Button::builder()
        .icon_name("mail-send-symbolic")
        .css_classes(["suggested-action"])
        .tooltip_text("Send text to device")
        .build();

    bottom_hbox.append(&entry);
    bottom_hbox.append(&send_btn);
    content_vbox.append(&bottom_hbox);

    let daemon_clone = daemon.clone();
    let id_clone = device_id_rc.clone();
    let entry_weak = entry.downgrade();
    entry.connect_activate(move |_| {
        if let Some(entry) = entry_weak.upgrade() {
            let text = entry.text().to_string();
            if !text.is_empty() {
                let mut payload = Map::new();
                payload.insert("key".to_string(), Value::String(text));
                daemon_clone.send(DaemonCommand::SendMousepadRequest(
                    id_clone.to_string(),
                    payload,
                ));
                entry.set_text("");
            }
        }
    });

    let daemon_clone2 = daemon.clone();
    let id_clone2 = device_id_rc.clone();
    let entry_weak2 = entry.downgrade();
    send_btn.connect_clicked(move |_| {
        if let Some(entry) = entry_weak2.upgrade() {
            let text = entry.text().to_string();
            if !text.is_empty() {
                let mut payload = Map::new();
                payload.insert("key".to_string(), Value::String(text));
                daemon_clone2.send(DaemonCommand::SendMousepadRequest(
                    id_clone2.to_string(),
                    payload,
                ));
                entry.set_text("");
            }
        }
    });

    window.set_content(Some(&content_vbox));
    window.present();
    trackpad_area.grab_focus();
}

fn map_gdk_key_to_special(keyval: Key) -> i32 {
    match keyval {
        Key::BackSpace => 1,
        Key::Tab => 2,
        Key::Left => 4,
        Key::Up => 5,
        Key::Right => 6,
        Key::Down => 7,
        Key::Page_Up => 8,
        Key::Page_Down => 9,
        Key::Home => 10,
        Key::End => 11,
        Key::Return | Key::KP_Enter => 12,
        Key::Delete => 13,
        Key::Escape => 14,
        Key::Sys_Req => 15,
        Key::Scroll_Lock => 16,
        Key::F1 => 21,
        Key::F2 => 22,
        Key::F3 => 23,
        Key::F4 => 24,
        Key::F5 => 25,
        Key::F6 => 26,
        Key::F7 => 27,
        Key::F8 => 28,
        Key::F9 => 29,
        Key::F10 => 30,
        Key::F11 => 31,
        Key::F12 => 32,
        _ => 0,
    }
}

fn click_payload(button: u32) -> Option<Map<String, Value>> {
    let key = match button {
        gdk::BUTTON_PRIMARY => "singleclick",
        gdk::BUTTON_SECONDARY => "rightclick",
        gdk::BUTTON_MIDDLE => "middleclick",
        _ => return None,
    };

    let mut payload = Map::new();
    payload.insert(key.to_string(), Value::Bool(true));
    Some(payload)
}

const DEFAULT_REMOTE_PHONE_WIDTH: i32 = 1080;
const DEFAULT_REMOTE_PHONE_HEIGHT: i32 = 2400;

fn map_preview_position(
    x: f64,
    y: f64,
    preview_width: i32,
    preview_height: i32,
    remote_width: i32,
    remote_height: i32,
) -> Option<(i32, i32)> {
    if preview_width <= 0 || preview_height <= 0 || remote_width <= 0 || remote_height <= 0 {
        return None;
    }

    let remote_x = ((x / f64::from(preview_width)) * f64::from(remote_width))
        .round()
        .clamp(0.0, f64::from(remote_width - 1)) as i32;
    let remote_y = ((y / f64::from(preview_height)) * f64::from(remote_height))
        .round()
        .clamp(0.0, f64::from(remote_height - 1)) as i32;
    Some((remote_x, remote_y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload_has_only_bool(payload: Map<String, Value>, key: &str) {
        assert_eq!(payload.len(), 1);
        assert_eq!(payload.get(key), Some(&Value::Bool(true)));
    }

    #[test]
    fn primary_click_sends_singleclick_not_drag_hold() {
        let payload = click_payload(gdk::BUTTON_PRIMARY).unwrap();

        assert!(!payload.contains_key("singlehold"));
        assert!(!payload.contains_key("singlerelease"));
        payload_has_only_bool(payload, "singleclick");
    }

    #[test]
    fn secondary_and_middle_clicks_match_android_mouse_receiver_fields() {
        payload_has_only_bool(click_payload(gdk::BUTTON_SECONDARY).unwrap(), "rightclick");
        payload_has_only_bool(click_payload(gdk::BUTTON_MIDDLE).unwrap(), "middleclick");
        assert!(click_payload(8).is_none());
    }

    #[test]
    fn preview_position_maps_to_remote_screen_coordinates() {
        assert_eq!(
            map_preview_position(200.0, 400.0, 400, 800, 1080, 2400),
            Some((540, 1200))
        );
    }

    #[test]
    fn preview_position_clamps_to_remote_screen_bounds() {
        assert_eq!(
            map_preview_position(999.0, -10.0, 400, 800, 1080, 2400),
            Some((1079, 0))
        );
    }
}
