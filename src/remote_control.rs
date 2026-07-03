use adw::prelude::*;
use gtk::gdk;
use gtk::gdk::Key;
use gtk::glib;
use serde_json::{Map, Value};
use std::cell::Cell;
use std::rc::Rc;

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
            border-radius: 16px;
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
        .label("⚪ Click trackpad to type directly")
        .css_classes(["status-pill"])
        .halign(gtk::Align::Center)
        .build();

    let trackpad_icon = gtk::Image::builder()
        .icon_name("input-touchpad-symbolic")
        .pixel_size(96)
        .halign(gtk::Align::Center)
        .opacity(0.5)
        .build();

    let label = gtk::Label::builder()
        .label("<b>Virtual Trackpad</b>\n\nHover mouse here to move cursor\nClick / tap to select or right-click\nScroll to scroll page")
        .use_markup(true)
        .justify(gtk::Justification::Center)
        .halign(gtk::Align::Center)
        .build();

    trackpad_vbox.append(&status_label);
    trackpad_vbox.append(&trackpad_icon);
    trackpad_vbox.append(&label);

    trackpad_area.append(&trackpad_vbox);
    content_vbox.append(&trackpad_area);

    let device_id_rc = Rc::new(device_id);

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
    motion.connect_motion(move |_, x, y| {
        let delta_x = x - lx.get();
        let delta_y = y - ly.get();
        lx.set(x);
        ly.set(y);

        if delta_x.abs() > 0.0001 || delta_y.abs() > 0.0001 {
            let mut payload = Map::new();
            payload.insert("dx".to_string(), Value::from(delta_x));
            payload.insert("dy".to_string(), Value::from(delta_y));
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
    click.connect_pressed(move |click, _, _, _| {
        if let Some(trackpad) = trackpad_weak.upgrade() {
            trackpad.grab_focus();
        }
        let button = click.current_button();
        if let Some(payload) = click_payload(button) {
            daemon_clone.send(DaemonCommand::SendMousepadRequest(
                id_clone.to_string(),
                payload,
            ));
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
            status.set_label("🟢 Keyboard Input Active");
            status.add_css_class("active");
        }
    });

    let status_weak2 = status_label.downgrade();
    focus_controller.connect_leave(move |_| {
        if let Some(status) = status_weak2.upgrade() {
            status.set_label("⚪ Click trackpad to type directly");
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
}
