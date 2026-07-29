use adw::prelude::*;
use gtk::gdk;
use gtk::gdk::Key;
use gtk::glib;
use serde_json::{Map, Value};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::webrtc::ScreenDirection;

pub fn show_remote_control_dialog(
    daemon: DaemonHandle,
    device_id: String,
    parent: &impl IsA<gtk::Window>,
) {
    // A remote view is always requested before the local user may ask to
    // inject input. The daemon enforces the actual grant independently of
    // this UI, so a stale dialog cannot send a usable mouse packet.
    daemon.send(DaemonCommand::RequestRemoteView(
        device_id.clone(),
        ScreenDirection::PhoneToDesktop,
    ));
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
    let request_view = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Request phone screen")
        .build();
    let daemon_clone = daemon.clone();
    let view_device_id = device_id.clone();
    request_view.connect_clicked(move |_| {
        daemon_clone.send(DaemonCommand::RequestRemoteView(
            view_device_id.clone(),
            ScreenDirection::PhoneToDesktop,
        ));
    });
    header.pack_start(&request_view);

    let enable_control = gtk::Button::builder()
        .label("Enable control")
        .tooltip_text("Request permission to control this phone")
        .css_classes(["suggested-action"])
        .build();
    let daemon_clone = daemon.clone();
    let control_device_id = device_id.clone();
    enable_control.connect_clicked(move |_| {
        daemon_clone.send(DaemonCommand::RequestRemoteControl(control_device_id.clone()));
    });
    header.pack_end(&enable_control);

    let stop = gtk::Button::builder()
        .label("Stop")
        .tooltip_text("Stop viewing and release remote control")
        .build();
    let daemon_clone = daemon.clone();
    let stop_device_id = device_id.clone();
    stop.connect_clicked(move |_| {
        daemon_clone.send(DaemonCommand::StopRemoteSession(stop_device_id.clone()));
    });
    header.pack_end(&stop);
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
        .hexpand(true)
        .vexpand(true)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();

    let remote_picture = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Contain)
        .hexpand(true)
        .vexpand(true)
        .visible(false)
        .build();

    let status_label = gtk::Label::builder()
        .label("Requesting the phone screen. Enable control only after viewing starts.")
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
        .label("<b>Remote phone screen</b>\n\nClick the visible phone frame to tap the matching phone coordinate.\nUse the keyboard, scroll wheel, or the phone navigation actions below.")
        .use_markup(true)
        .justify(gtk::Justification::Center)
        .halign(gtk::Align::Center)
        .build();

    trackpad_vbox.append(&remote_picture);
    trackpad_vbox.append(&status_label);
    trackpad_vbox.append(&trackpad_icon);
    trackpad_vbox.append(&label);

    trackpad_area.append(&trackpad_vbox);
    content_vbox.append(&trackpad_area);

    let device_id_rc = Rc::new(device_id);

    // The WebRTC peer can be replaced while this dialog remains open. Look up
    // the bounded receiver on every GTK frame tick so a new generation starts
    // rendering immediately and a stopped/revoked stream clears its last
    // image rather than leaving a misleading stale screenshot.
    let picture = remote_picture.downgrade();
    let status = status_label.downgrade();
    let icon = trackpad_icon.downgrade();
    let instructions = label.downgrade();
    let had_frame = Rc::new(Cell::new(false));
    let frame_size = Rc::new(Cell::new((0_i32, 0_i32)));
    let input_size = Rc::new(Cell::new((0_i32, 0_i32)));
    let frame_device_id = Rc::clone(&device_id_rc);
    let frame_state = Rc::clone(&had_frame);
    let frame_dimensions = Rc::clone(&frame_size);
    let input_dimensions = Rc::clone(&input_size);
    glib::timeout_add_local(Duration::from_millis(33), move || {
        let Some(picture) = picture.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let latest = crate::device_links::webrtc::video_receive::receiver(frame_device_id.as_ref())
            .and_then(|receiver| {
                let receiver = receiver.lock().ok()?;
                let mut latest = None;
                loop {
                    match receiver.try_recv() {
                        Ok(frame) => latest = Some(frame),
                        Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                    }
                }
                latest
            });
        if let Some(frame) = latest {
            let decoded_size = (frame.width, frame.height);
            frame_dimensions.set(decoded_size);
            input_dimensions.set(
                crate::device_links::webrtc::video_receive::input_size(frame_device_id.as_ref())
                    .unwrap_or(decoded_size),
            );
            let texture = gdk::MemoryTexture::new(
                frame.width,
                frame.height,
                gdk::MemoryFormat::R8g8b8a8,
                &glib::Bytes::from_owned(frame.rgba),
                frame.stride,
            );
            picture.set_paintable(Some(&texture));
            picture.set_visible(true);
            frame_state.set(true);
            if let Some(status) = status.upgrade() {
                status.set_label("Viewing phone screen over WebRTC");
            }
            if let Some(icon) = icon.upgrade() {
                icon.set_visible(false);
            }
            if let Some(instructions) = instructions.upgrade() {
                instructions.set_visible(false);
            }
        } else if frame_state.replace(false) {
            frame_dimensions.set((0, 0));
            input_dimensions.set((0, 0));
            picture.set_paintable(None::<&gdk::Texture>);
            picture.set_visible(false);
            if let Some(status) = status.upgrade() {
                status.set_label("Screen sharing paused. Retry after the phone is unlocked.");
            }
            if let Some(icon) = icon.upgrade() {
                icon.set_visible(true);
            }
            if let Some(instructions) = instructions.upgrade() {
                instructions.set_visible(true);
            }
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
    let motion_area = trackpad_area.downgrade();
    let motion_frame_size = Rc::clone(&frame_size);
    let motion_input_size = Rc::clone(&input_size);
    motion.connect_motion(move |_, x, y| {
        let delta_x = x - lx.get();
        let delta_y = y - ly.get();
        lx.set(x);
        ly.set(y);

        let absolute = motion_area
            .upgrade()
            .and_then(|area| {
                screen_point_payload(
                    x,
                    y,
                    area.allocated_width(),
                    area.allocated_height(),
                    motion_frame_size.get(),
                    motion_input_size.get(),
                )
            });
        if let Some(payload) = absolute {
            daemon_clone.send(DaemonCommand::SendMousepadRequest(
                id_clone.to_string(),
                payload,
            ));
        } else if delta_x.abs() > 0.0001 || delta_y.abs() > 0.0001 {
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
    let trackpad_weak = trackpad_area.downgrade();
    click.connect_pressed(move |_, _, _, _| {
        if let Some(trackpad) = trackpad_weak.upgrade() {
            trackpad.grab_focus();
        }
    });

    let daemon_clone = daemon.clone();
    let id_clone = device_id_rc.clone();
    let trackpad_weak = trackpad_area.downgrade();
    let click_frame_size = Rc::clone(&frame_size);
    let click_input_size = Rc::clone(&input_size);
    let suppress_click = Rc::new(Cell::new(false));
    let suppress_click_release = Rc::clone(&suppress_click);
    click.connect_released(move |click, _, x, y| {
        if suppress_click_release.replace(false) {
            return;
        }
        let button = click.current_button();
        let payload = trackpad_weak.upgrade().and_then(|area| {
            if button == gdk::BUTTON_PRIMARY {
                screen_point_payload(
                    x,
                    y,
                    area.allocated_width(),
                    area.allocated_height(),
                    click_frame_size.get(),
                    click_input_size.get(),
                )
                .map(|mut payload| {
                    payload.insert("singleclick".to_string(), Value::Bool(true));
                    payload
                })
                .or_else(|| click_payload(button))
            } else {
                click_payload(button)
            }
        });
        if let Some(payload) = payload {
            daemon_clone.send(DaemonCommand::SendMousepadRequest(
                id_clone.to_string(),
                payload,
            ));
        }
    });

    trackpad_area.add_controller(click);

    // A drag is a reliable press → replaceable absolute moves → reliable
    // release sequence. Android's accessibility service holds the gesture
    // until it receives the explicit release, so a peer replacement cannot
    // leave a finger pressed on the phone.
    let drag = gtk::GestureDrag::new();
    drag.set_button(gdk::BUTTON_PRIMARY);
    let drag_origin = Rc::new(Cell::new((0.0, 0.0)));
    let drag_area = trackpad_area.downgrade();
    let drag_frame_size = Rc::clone(&frame_size);
    let drag_input_size = Rc::clone(&input_size);
    let drag_daemon = daemon.clone();
    let drag_device_id = Rc::clone(&device_id_rc);
    let drag_suppress_click = Rc::clone(&suppress_click);
    let drag_origin_begin = Rc::clone(&drag_origin);
    drag.connect_drag_begin(move |_, x, y| {
        drag_suppress_click.set(true);
        drag_origin_begin.set((x, y));
        let payload = drag_area.upgrade().and_then(|area| {
            screen_point_payload(
                x,
                y,
                area.allocated_width(),
                area.allocated_height(),
                drag_frame_size.get(),
                drag_input_size.get(),
            )
            .map(|mut payload| {
                payload.insert("singlehold".to_string(), Value::Bool(true));
                payload
            })
        });
        if let Some(payload) = payload {
            drag_daemon.send(DaemonCommand::SendMousepadRequest(
                drag_device_id.to_string(),
                payload,
            ));
        }
    });
    let drag_area = trackpad_area.downgrade();
    let drag_frame_size = Rc::clone(&frame_size);
    let drag_input_size = Rc::clone(&input_size);
    let drag_daemon = daemon.clone();
    let drag_device_id = Rc::clone(&device_id_rc);
    let drag_origin_update = Rc::clone(&drag_origin);
    drag.connect_drag_update(move |_, offset_x, offset_y| {
        let (start_x, start_y) = drag_origin_update.get();
        let payload = drag_area
            .upgrade()
            .and_then(|area| {
                screen_point_payload(
                    start_x + offset_x,
                    start_y + offset_y,
                    area.allocated_width(),
                    area.allocated_height(),
                    drag_frame_size.get(),
                    drag_input_size.get(),
                )
            })
            .unwrap_or_else(|| {
                let mut payload = Map::new();
                payload.insert("dx".to_string(), Value::from(offset_x));
                payload.insert("dy".to_string(), Value::from(offset_y));
                payload
            });
        drag_daemon.send(DaemonCommand::SendMousepadRequest(
            drag_device_id.to_string(),
            payload,
        ));
    });
    let drag_area = trackpad_area.downgrade();
    let drag_frame_size = Rc::clone(&frame_size);
    let drag_input_size = Rc::clone(&input_size);
    let drag_daemon = daemon.clone();
    let drag_device_id = Rc::clone(&device_id_rc);
    let drag_origin_end = Rc::clone(&drag_origin);
    drag.connect_drag_end(move |_, offset_x, offset_y| {
        let (start_x, start_y) = drag_origin_end.get();
        let mut payload = drag_area
            .upgrade()
            .and_then(|area| {
                screen_point_payload(
                    start_x + offset_x,
                    start_y + offset_y,
                    area.allocated_width(),
                    area.allocated_height(),
                    drag_frame_size.get(),
                    drag_input_size.get(),
                )
            })
            .unwrap_or_default();
        payload.insert("singlerelease".to_string(), Value::Bool(true));
        drag_daemon.send(DaemonCommand::SendMousepadRequest(
            drag_device_id.to_string(),
            payload,
        ));
    });
    trackpad_area.add_controller(drag);

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

    // Android has no useful desktop-style right/middle click semantics in a
    // direct phone-control session. Present explicit navigation actions
    // instead; the Android accessibility receiver maps these to Back, Home,
    // and Recents after it validates the same WebRTC control lease.
    let phone_actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(20)
        .margin_end(20)
        .build();
    for (label, field, tooltip) in [
        ("Back", "backclick", "Android Back"),
        ("Home", "middleclick", "Android Home"),
        ("Recents", "forwardclick", "Android Recents"),
    ] {
        let button = gtk::Button::builder()
            .label(label)
            .tooltip_text(tooltip)
            .hexpand(true)
            .build();
        let daemon = daemon.clone();
        let device_id = Rc::clone(&device_id_rc);
        button.connect_clicked(move |_| {
            let mut payload = Map::new();
            payload.insert(field.to_string(), Value::Bool(true));
            daemon.send(DaemonCommand::SendMousepadRequest(device_id.to_string(), payload));
        });
        phone_actions.append(&button);
    }
    content_vbox.append(&phone_actions);

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
    let daemon_for_close = daemon.clone();
    let device_for_close = device_id_rc.clone();
    window.connect_close_request(move |_| {
        daemon_for_close.send(DaemonCommand::StopRemoteSession(device_for_close.to_string()));
        glib::Propagation::Proceed
    });
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
    if button != gdk::BUTTON_PRIMARY {
        return None;
    }

    let mut payload = Map::new();
    payload.insert("singleclick".to_string(), Value::Bool(true));
    Some(payload)
}

/// Converts a pointer position in the GTK viewer into the native Android
/// display coordinate advertised with `screen-ready`. The decoded VP8 frame
/// can be downscaled and letterboxed, so raw GTK coordinates are never sent
/// straight to AccessibilityService.
fn screen_point_payload(
    x: f64,
    y: f64,
    viewport_width: i32,
    viewport_height: i32,
    decoded_size: (i32, i32),
    target_size: (i32, i32),
) -> Option<Map<String, Value>> {
    let (decoded_width, decoded_height) = decoded_size;
    let (target_width, target_height) = target_size;
    if viewport_width <= 0
        || viewport_height <= 0
        || decoded_width <= 0
        || decoded_height <= 0
        || target_width <= 0
        || target_height <= 0
    {
        return None;
    }
    let scale = f64::min(
        f64::from(viewport_width) / f64::from(decoded_width),
        f64::from(viewport_height) / f64::from(decoded_height),
    );
    let visible_width = f64::from(decoded_width) * scale;
    let visible_height = f64::from(decoded_height) * scale;
    let left = (f64::from(viewport_width) - visible_width) / 2.0;
    let top = (f64::from(viewport_height) - visible_height) / 2.0;
    if x < left || x > left + visible_width || y < top || y > top + visible_height {
        return None;
    }
    let mapped_x = ((x - left) / visible_width * f64::from(target_width))
        .round()
        .clamp(0.0, f64::from(target_width - 1)) as i32;
    let mapped_y = ((y - top) / visible_height * f64::from(target_height))
        .round()
        .clamp(0.0, f64::from(target_height - 1)) as i32;
    let mut payload = Map::new();
    payload.insert("x".to_string(), Value::from(mapped_x));
    payload.insert("y".to_string(), Value::from(mapped_y));
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
    fn desktop_style_secondary_clicks_are_not_presented_as_phone_controls() {
        assert!(click_payload(gdk::BUTTON_SECONDARY).is_none());
        assert!(click_payload(gdk::BUTTON_MIDDLE).is_none());
        assert!(click_payload(8).is_none());
    }

    #[test]
    fn screen_click_maps_through_letterboxing_to_android_coordinates() {
        let payload = screen_point_payload(500.0, 500.0, 1000, 1000, (1000, 500), (2000, 1000))
            .unwrap();
        // 1000×500 is vertically centered inside a 1000×1000 viewport, so
        // the point is halfway across and halfway down the visible frame.
        assert_eq!(payload.get("x"), Some(&Value::from(1000)));
        assert_eq!(payload.get("y"), Some(&Value::from(500)));
    }
}
