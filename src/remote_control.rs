//! The phone's screen in a window, scrcpy-style: the live picture fills it,
//! and the mouse acts on the phone directly where it points — click to tap,
//! drag to swipe, hold to long-press, scroll to scroll — with the keyboard
//! typing into the phone. Right click is Back, middle click is Home, and the
//! header carries Back / Home / Recents.

use adw::prelude::*;
use gtk::gdk;
use gtk::gdk::Key;
use gtk::glib;
use serde_json::{Map, Value};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::webrtc::remote_session::SessionState;

/// A press that moves less than this (in window pixels) is a tap, not a swipe.
const TAP_SLOP: f64 = 8.0;
/// A still press held this long is a long-press.
const LONG_PRESS: std::time::Duration = std::time::Duration::from_millis(500);

/// Where the video is drawn inside a widget when scaled to fit (letterboxed),
/// and where a point on the widget falls on the phone's own screen. `None`
/// outside the picture (the black bars).
pub(crate) fn map_to_phone(
    point: (f64, f64),
    widget: (f64, f64),
    video: (f64, f64),
    phone: (i32, i32),
) -> Option<(i32, i32)> {
    let (ww, wh) = widget;
    let (vw, vh) = video;
    if ww <= 0.0 || wh <= 0.0 || vw <= 0.0 || vh <= 0.0 || phone.0 <= 0 || phone.1 <= 0 {
        return None;
    }
    let scale = (ww / vw).min(wh / vh);
    let (dw, dh) = (vw * scale, vh * scale);
    let (ox, oy) = ((ww - dw) / 2.0, (wh - dh) / 2.0);
    let fx = (point.0 - ox) / dw;
    let fy = (point.1 - oy) / dh;
    if !(0.0..=1.0).contains(&fx) || !(0.0..=1.0).contains(&fy) {
        return None;
    }
    let x = (fx * f64::from(phone.0)).round() as i32;
    let y = (fy * f64::from(phone.1)).round() as i32;
    Some((x.clamp(0, phone.0 - 1), y.clamp(0, phone.1 - 1)))
}

/// What a finished left-button press means: a drag that moved is a swipe
/// (already streaming), a still one is a tap or, if held, a long-press.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Press {
    Tap,
    Hold,
    SwipeEnded,
}

pub(crate) fn classify_press(moved: bool, held: std::time::Duration) -> Press {
    if moved {
        Press::SwipeEnded
    } else if held >= LONG_PRESS {
        Press::Hold
    } else {
        Press::Tap
    }
}

fn action(name: &str) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert(name.to_string(), Value::Bool(true));
    payload
}

fn at(mut payload: Map<String, Value>, (x, y): (i32, i32)) -> Map<String, Value> {
    payload.insert("x".to_string(), Value::from(x));
    payload.insert("y".to_string(), Value::from(y));
    payload
}

thread_local! {
    static OPEN_WINDOWS: RefCell<std::collections::HashMap<String, glib::WeakRef<adw::Window>>> =
        RefCell::new(std::collections::HashMap::new());
}

/// How to turn on the phone's control, shown when it refuses for that. A
/// sideloaded app's accessibility switch is greyed out until Android's
/// "restricted settings" are allowed for it.
const ACCESSIBILITY_STEPS: &str = "On the phone: Settings → Accessibility → DeskLink → On \
     (if it's greyed out: App info → ⋮ → Allow restricted settings first)";

pub fn show_remote_control_dialog(
    daemon: DaemonHandle,
    device_id: String,
    parent: &impl IsA<gtk::Window>,
) {
    // One window per phone: a second would start the session again, and
    // closing either would stop the other's.
    if let Some(open) = OPEN_WINDOWS.with(|open| {
        open.borrow()
            .get(&device_id)
            .and_then(|window| window.upgrade())
    }) {
        open.present();
        return;
    }
    let device = Rc::new(device_id);

    // Phone-shaped by default; the picture keeps its aspect when resized.
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(false)
        .default_width(420)
        .default_height(900)
        .title("Phone Screen")
        .css_classes(["remote-window"])
        .build();

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let send = {
        let daemon = daemon.clone();
        let device = Rc::clone(&device);
        Rc::new(move |payload: Map<String, Value>| {
            if remote_control_lease_active(device.as_str()) {
                daemon.send(DaemonCommand::SendMousepadRequest(
                    device.to_string(),
                    payload,
                ));
            }
        })
    };
    // Some phones (ColorOS) black out the status bar in any screen capture, so
    // its essentials — time, battery, charging — are shown here instead.
    let title = adw::WindowTitle::new("Phone Screen", "");
    header.set_title_widget(Some(&title));
    // How the phone is connected: local network, direct, or relayed.
    let route = gtk::Label::builder()
        .css_classes(["caption", "dim-label"])
        .valign(gtk::Align::Center)
        .visible(false)
        .build();
    // Which kind of device this is: phone, tablet, …
    let kind = gtk::Image::builder()
        .icon_name("desklink-device-phone-symbolic")
        .valign(gtk::Align::Center)
        .build();
    header.pack_start(&kind);
    header.pack_start(&route);
    toolbar.add_top_bar(&header);

    // The phone's navigation keys in a floating pill under the picture, where
    // they never cover the phone's own screen.
    let keys = gtk::Box::builder()
        .spacing(6)
        .halign(gtk::Align::Center)
        .css_classes(["toolbar", "osd", "floating-toolbar"])
        .build();
    for (icon, tooltip, name) in [
        ("go-previous-symbolic", "Back (right click)", "backclick"),
        ("go-home-symbolic", "Home (middle click)", "middleclick"),
        ("view-app-grid-symbolic", "Recent Apps", "forwardclick"),
    ] {
        let button = gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(tooltip)
            .css_classes(["circular"])
            .build();
        button.update_property(&[gtk::accessible::Property::Label(tooltip)]);
        let send = Rc::clone(&send);
        button.connect_clicked(move |_| send(action(name)));
        keys.append(&button);
    }
    keys.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    // The phone's sound here while its screen shows (F1).
    let speaker = gtk::ToggleButton::builder()
        .icon_name("audio-volume-muted-symbolic")
        .tooltip_text("Play the Phone's Sound Here")
        .css_classes(["circular"])
        .build();
    speaker.update_property(&[gtk::accessible::Property::Label(
        "Play the Phone's Sound Here",
    )]);
    {
        let (daemon, device) = (daemon.clone(), Rc::clone(&device));
        speaker.connect_toggled(move |speaker| {
            let on = speaker.is_active();
            speaker.set_icon_name(if on {
                "audio-volume-high-symbolic"
            } else {
                "audio-volume-muted-symbolic"
            });
            daemon.send(DaemonCommand::Sound(
                device.to_string(),
                "phone-sound".into(),
                on,
            ));
        });
    }
    keys.append(&speaker);
    let fullscreen = gtk::Button::builder()
        .icon_name("view-fullscreen-symbolic")
        .tooltip_text("Full Screen")
        .css_classes(["circular"])
        .build();
    fullscreen.update_property(&[gtk::accessible::Property::Label("Full Screen")]);
    {
        let window = window.downgrade();
        fullscreen.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                if window.is_fullscreen() {
                    window.unfullscreen();
                } else {
                    window.fullscreen();
                }
            }
        });
    }
    keys.append(&fullscreen);
    toolbar.add_bottom_bar(&keys);
    {
        let (toolbar, fullscreen) = (toolbar.downgrade(), fullscreen.downgrade());
        window.connect_fullscreened_notify(move |window| {
            let full = window.is_fullscreen();
            if let Some(toolbar) = toolbar.upgrade() {
                toolbar.set_reveal_top_bars(!full);
            }
            if let Some(button) = fullscreen.upgrade() {
                let label = if full {
                    "Leave Full Screen"
                } else {
                    "Full Screen"
                };
                button.set_icon_name(if full {
                    "view-restore-symbolic"
                } else {
                    "view-fullscreen-symbolic"
                });
                button.set_tooltip_text(Some(label));
                button.update_property(&[gtk::accessible::Property::Label(label)]);
            }
        });
    }

    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .can_shrink(true)
        .hexpand(true)
        .vexpand(true)
        .focusable(true)
        .css_classes(["phone-screen"])
        .build();
    gtk::prelude::AccessibleExtManual::update_property(
        &picture,
        &[gtk::accessible::Property::Label("Phone screen")],
    );
    // A note over the picture once there is one ("Asking the phone for
    // control…"); before that, a centred spinner with the same words.
    let status = gtk::Label::builder()
        .css_classes(["osd", "toolbar"])
        .wrap(true)
        .justify(gtk::Justification::Center)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .margin_top(12)
        .build();
    let loading = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    loading.append(
        &gtk::Spinner::builder()
            .spinning(true)
            .width_request(32)
            .height_request(32)
            .build(),
    );
    let loading_label = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .max_width_chars(32)
        .css_classes(["dim-label"])
        .build();
    loading.append(&loading_label);
    // After the phone declined or stopped sharing: nothing asks again by
    // itself; the person does, here.
    let ask_again = gtk::Button::builder()
        .label("Ask Again")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .visible(false)
        .build();
    loading.append(&ask_again);
    let overlay = gtk::Overlay::builder().child(&picture).build();
    overlay.add_overlay(&loading);
    overlay.add_overlay(&status);
    toolbar.set_content(Some(&overlay));

    // A screen the phone offered is already being started under its id.
    let restarter = Rc::new(RefCell::new(Restarter::default()));
    if matches!(
        session_state(device.as_str()),
        None | Some(SessionState::Idle | SessionState::Stopped)
    ) {
        daemon.send(DaemonCommand::StartPhoneRemoteControl(device.to_string()));
        restarter.borrow_mut().sent(std::time::Instant::now());
    }
    {
        let (restarter, daemon, device) =
            (Rc::clone(&restarter), daemon.clone(), Rc::clone(&device));
        ask_again.connect_clicked(move |_| {
            restarter.borrow_mut().ask_again();
            daemon.send(DaemonCommand::StartPhoneRemoteControl(device.to_string()));
            restarter.borrow_mut().sent(std::time::Instant::now());
        });
    }
    let link_ready = Rc::new(Cell::new(true));
    let last_control_request = Rc::new(Cell::new(std::time::Instant::now()));

    // State, picture and status, refreshed ~30 times a second.
    {
        let device = Rc::clone(&device);
        let daemon = daemon.clone();
        let picture = picture.downgrade();
        let status = status.downgrade();
        let loading = loading.downgrade();
        let loading_label = loading_label.downgrade();
        let restarter = Rc::clone(&restarter);
        let link_ready = Rc::clone(&link_ready);
        let ask_again = ask_again.downgrade();
        let last_control_request = Rc::clone(&last_control_request);
        let title = title.downgrade();
        let route = route.downgrade();
        let kind = kind.downgrade();
        let tick = Cell::new(0u32);
        glib::timeout_add_local(std::time::Duration::from_millis(33), move || {
            let (Some(picture), Some(status)) = (picture.upgrade(), status.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            tick.set(tick.get().wrapping_add(1));
            if tick.get() % 30 == 1 {
                if let Some(title) = title.upgrade() {
                    if let Some(view) = daemon.devices().into_iter().find(|d| d.id == *device) {
                        link_ready.set(view.feature_transport_ready);
                        title.set_title(&view.name);
                        if let Some(kind) = kind.upgrade() {
                            kind.set_icon_name(Some(crate::window::device_type_icon(
                                &view.device_type,
                            )));
                            kind.set_tooltip_text(Some(&crate::window::device_type_label(
                                &view.device_type,
                            )));
                        }
                        if let Some(route) = route.upgrade() {
                            let text = view
                                .connection_path
                                .as_deref()
                                .and_then(crate::device_links::device::connection_path_label);
                            route.set_visible(text.is_some());
                            route.set_label(text.unwrap_or_default());
                        }
                        title.set_subtitle(&phone_status_line(
                            view.battery_status.as_ref(),
                            &glib::DateTime::now_local()
                                .and_then(|now| now.format("%H:%M"))
                                .map(|time| time.to_string())
                                .unwrap_or_default(),
                        ));
                    }
                }
            }
            let state = session_state(device.as_str());
            // Asked again only when nothing was refused: not after the phone
            // declined or stopped sharing (the person asks again, with the
            // button), only after a dropped link comes back.
            if restarter
                .borrow_mut()
                .step(state, link_ready.get(), std::time::Instant::now())
            {
                daemon.send(DaemonCommand::StartPhoneRemoteControl(device.to_string()));
            }
            let gave_up = restarter.borrow().gave_up();
            if let Some(button) = ask_again.upgrade() {
                button.set_visible(gave_up);
            }
            // Viewing but not controlling: the phone refused (usually its
            // accessibility service is off) or the request was lost. Keep
            // asking, so turning the service on is all the user has to do.
            if state == Some(SessionState::Viewing)
                && last_control_request.get().elapsed() >= std::time::Duration::from_secs(2)
            {
                daemon.send(DaemonCommand::RequestPhoneControl(device.to_string()));
                last_control_request.set(std::time::Instant::now());
            }
            let denial = session_control_denial(device.as_str())
                .map(|reason| format!("{reason} — {ACCESSIBILITY_STEPS}"));
            let paintable = crate::device_links::webrtc::screen::stream_for(device.as_str())
                .and_then(|stream| stream.paintable());
            if picture.paintable() != paintable {
                picture.set_paintable(paintable.as_ref());
            }
            #[cfg(debug_assertions)]
            save_frame_on_request(&picture);
            let text = match state {
                Some(SessionState::RequestingView) => Some("Approve screen sharing on the phone…"),
                Some(SessionState::Viewing) | Some(SessionState::RequestingControl)
                    if denial.is_some() =>
                {
                    denial.as_deref()
                }
                Some(SessionState::Viewing) | Some(SessionState::RequestingControl) => {
                    Some("Asking the phone for control…")
                }
                Some(SessionState::Controlling) if paintable.is_none() => {
                    Some("Allow screen sharing on the phone (check its notifications)…")
                }
                Some(SessionState::Controlling) => None,
                Some(SessionState::PausedLocked) => Some("Phone is locked · unlock it to continue"),
                Some(SessionState::Stopped) if gave_up => {
                    Some("The phone declined or stopped sharing its screen")
                }
                Some(SessionState::Stopped) => Some("Screen stopped · reconnecting…"),
                _ => None,
            };
            let connecting = format!(
                "Connecting to {}…",
                title
                    .upgrade()
                    .map(|title| title.title().to_string())
                    .filter(|name| !name.is_empty() && name != "Phone Screen")
                    .unwrap_or_else(|| "the phone".to_string())
            );
            let waiting = paintable.is_none();
            if let (Some(loading), Some(label)) = (loading.upgrade(), loading_label.upgrade()) {
                loading.set_visible(waiting);
                label.set_label(text.unwrap_or(&connecting));
            }
            status.set_visible(!waiting && text.is_some());
            if let Some(text) = text {
                status.set_label(text);
            }
            glib::ControlFlow::Continue
        });
    }

    {
        let daemon = daemon.clone();
        let device = Rc::clone(&device);
        let speaker = speaker.downgrade();
        window.connect_close_request(move |_| {
            OPEN_WINDOWS.with(|open| open.borrow_mut().remove(device.as_str()));
            if speaker.upgrade().is_some_and(|speaker| speaker.is_active()) {
                daemon.send(DaemonCommand::Sound(
                    device.to_string(),
                    "phone-sound".into(),
                    false,
                ));
            }
            daemon.send(DaemonCommand::StopPhoneRemoteControl(device.to_string()));
            glib::Propagation::Proceed
        });
    }
    OPEN_WINDOWS.with(|open| {
        open.borrow_mut()
            .insert(device.to_string(), window.downgrade())
    });

    // Window point → phone pixel, using the picture's current layout.
    let to_phone = {
        let picture = picture.downgrade();
        let device = Rc::clone(&device);
        Rc::new(move |x: f64, y: f64| -> Option<(i32, i32)> {
            let picture = picture.upgrade()?;
            let paintable = picture.paintable()?;
            let phone = session_phone_screen(device.as_str())?;
            map_to_phone(
                (x, y),
                (f64::from(picture.width()), f64::from(picture.height())),
                (
                    f64::from(paintable.intrinsic_width()),
                    f64::from(paintable.intrinsic_height()),
                ),
                phone,
            )
        })
    };

    // Left button: tap, long-press, or a swipe that follows the pointer.
    {
        let drag = gtk::GestureDrag::builder()
            .button(gdk::BUTTON_PRIMARY)
            .build();
        let start = Rc::new(Cell::new((0.0, 0.0)));
        let pressed_at = Rc::new(Cell::new(std::time::Instant::now()));
        let swiping = Rc::new(Cell::new(false));
        let last = Rc::new(RefCell::new(None::<(i32, i32)>));
        {
            let (start, pressed_at, swiping) = (
                Rc::clone(&start),
                Rc::clone(&pressed_at),
                Rc::clone(&swiping),
            );
            // The gesture's own widget, not a captured `picture`: the
            // gesture is attached to the picture, so that would be a cycle.
            drag.connect_drag_begin(move |gesture, x, y| {
                if let Some(picture) = gesture.widget() {
                    picture.grab_focus();
                }
                start.set((x, y));
                pressed_at.set(std::time::Instant::now());
                swiping.set(false);
            });
        }
        {
            let (start, swiping, last, to_phone, send) = (
                Rc::clone(&start),
                Rc::clone(&swiping),
                Rc::clone(&last),
                Rc::clone(&to_phone),
                Rc::clone(&send),
            );
            drag.connect_drag_update(move |_, dx, dy| {
                let (sx, sy) = start.get();
                if !swiping.get() {
                    if dx.hypot(dy) < TAP_SLOP {
                        return;
                    }
                    let Some(origin) = to_phone(sx, sy) else {
                        return;
                    };
                    swiping.set(true);
                    send(at(action("singlehold"), origin));
                }
                if let Some(point) = to_phone(sx + dx, sy + dy) {
                    if *last.borrow() != Some(point) {
                        *last.borrow_mut() = Some(point);
                        send(at(Map::new(), point));
                    }
                }
            });
        }
        {
            let (start, pressed_at, swiping, last, to_phone, send) = (
                Rc::clone(&start),
                Rc::clone(&pressed_at),
                Rc::clone(&swiping),
                Rc::clone(&last),
                Rc::clone(&to_phone),
                Rc::clone(&send),
            );
            drag.connect_drag_end(move |_, dx, dy| {
                let (sx, sy) = start.get();
                last.borrow_mut().take();
                match classify_press(swiping.get(), pressed_at.get().elapsed()) {
                    Press::SwipeEnded => {
                        if let Some(point) = to_phone(sx + dx, sy + dy) {
                            send(at(Map::new(), point));
                        }
                        send(action("singlerelease"));
                    }
                    Press::Hold => {
                        if let Some(point) = to_phone(sx, sy) {
                            send(at(action("singlehold"), point));
                            send(action("singlerelease"));
                        }
                    }
                    Press::Tap => {
                        if let Some(point) = to_phone(sx, sy) {
                            send(at(action("singleclick"), point));
                        }
                    }
                }
                swiping.set(false);
            });
        }
        picture.add_controller(drag);
    }

    // Right click is Back, middle click is Home — scrcpy's mapping.
    {
        let click = gtk::GestureClick::builder().button(0).build();
        let send = Rc::clone(&send);
        click.connect_pressed(move |gesture, _, _, _| match gesture.current_button() {
            gdk::BUTTON_SECONDARY => send(action("backclick")),
            gdk::BUTTON_MIDDLE => send(action("middleclick")),
            _ => {}
        });
        picture.add_controller(click);
    }

    // Scroll where the pointer is.
    {
        let pointer = Rc::new(Cell::new((0.0, 0.0)));
        let motion = gtk::EventControllerMotion::new();
        {
            let pointer = Rc::clone(&pointer);
            motion.connect_motion(move |_, x, y| pointer.set((x, y)));
        }
        picture.add_controller(motion);
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        let (send, to_phone) = (Rc::clone(&send), Rc::clone(&to_phone));
        scroll.connect_scroll(move |_, dx, dy| {
            let (x, y) = pointer.get();
            let Some(point) = to_phone(x, y) else {
                return glib::Propagation::Proceed;
            };
            send(at(Map::new(), point));
            let mut payload = action("scroll");
            payload.insert("dx".to_string(), Value::from(-dx * 3.0));
            payload.insert("dy".to_string(), Value::from(-dy * 3.0));
            send(payload);
            glib::Propagation::Stop
        });
        picture.add_controller(scroll);
    }

    // Typing goes straight to the phone.
    {
        let keys = gtk::EventControllerKey::new();
        let send = Rc::clone(&send);
        keys.connect_key_pressed(move |_, keyval, _, state| {
            let mut payload = Map::new();
            payload.insert(
                "shift".to_string(),
                Value::Bool(state.contains(gdk::ModifierType::SHIFT_MASK)),
            );
            payload.insert(
                "ctrl".to_string(),
                Value::Bool(state.contains(gdk::ModifierType::CONTROL_MASK)),
            );
            payload.insert(
                "alt".to_string(),
                Value::Bool(state.contains(gdk::ModifierType::ALT_MASK)),
            );
            payload.insert(
                "super".to_string(),
                Value::Bool(state.contains(gdk::ModifierType::SUPER_MASK)),
            );
            let special = map_gdk_key_to_special(keyval);
            if special > 0 {
                payload.insert("specialKey".to_string(), Value::from(special));
            } else if let Some(text) = keyval.to_unicode().filter(|c| !c.is_control()) {
                payload.insert("key".to_string(), Value::String(text.to_string()));
            } else {
                return glib::Propagation::Proceed;
            }
            send(payload);
            glib::Propagation::Stop
        });
        window.add_controller(keys);
    }

    window.set_content(Some(&toolbar));
    window.present();
    picture.grab_focus();
}

/// Debug builds only: `touch $DESKLINK_FRAME_DIR/request` and the window
/// writes what it shows to `frame.png` there — the device test harness's eyes.
#[cfg(debug_assertions)]
fn save_frame_on_request(picture: &gtk::Picture) {
    let Ok(dir) = std::env::var("DESKLINK_FRAME_DIR") else {
        return;
    };
    let request = std::path::Path::new(&dir).join("request");
    if std::fs::remove_file(&request).is_err() {
        return;
    }
    let (Some(paintable), Some(native)) = (picture.paintable(), picture.native()) else {
        return;
    };
    let Some(renderer) = native.renderer() else {
        return;
    };
    let (width, height) = (paintable.intrinsic_width(), paintable.intrinsic_height());
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, f64::from(width), f64::from(height));
    if let Some(node) = snapshot.to_node() {
        let texture = renderer.render_texture(node, None);
        let _ = texture.save_to_png(std::path::Path::new(&dir).join("frame.png"));
    }
}

fn phone_status_line(
    battery: Option<&crate::device_links::device::BatteryStatus>,
    time: &str,
) -> String {
    match battery.filter(|battery| battery.current_charge >= 0) {
        Some(battery) => format!(
            "{time} · Battery {}%{}",
            battery.current_charge,
            if battery.is_charging {
                " ⚡ charging"
            } else {
                ""
            }
        ),
        None => time.to_string(),
    }
}

fn session_state(device_id: &str) -> Option<SessionState> {
    crate::device_links::webrtc::remote_session::sessions()
        .lock()
        .ok()?
        .get(device_id)
        .map(|session| session.state())
}

fn session_control_denial(device_id: &str) -> Option<String> {
    crate::device_links::webrtc::remote_session::sessions()
        .lock()
        .ok()?
        .get(device_id)?
        .control_denial()
        .map(str::to_string)
}

fn session_phone_screen(device_id: &str) -> Option<(i32, i32)> {
    crate::device_links::webrtc::remote_session::sessions()
        .lock()
        .ok()?
        .get(device_id)?
        .phone_screen()
}

fn remote_control_lease_active(device_id: &str) -> bool {
    crate::device_links::webrtc::coordinator::has_remote_control_lease(device_id)
}

pub(crate) fn map_gdk_key_to_special(keyval: Key) -> i32 {
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

/// When the Phone Screen window asks the phone to share its screen. It asks
/// at the start and, when the link dropped and came back, again; it never asks
/// again after the phone declined or stopped sharing (the consent dialog
/// would come back every few seconds): that is the person's choice, through
/// "Ask Again".
#[derive(Default)]
struct Restarter {
    last_ask: Option<std::time::Instant>,
    gave_up: bool,
    /// The session ended while the link was down: ask again when it is back.
    stopped_while_down: bool,
}

/// A fresh ask is left alone this long before the answer is judged.
const ASK_SETTLE: std::time::Duration = std::time::Duration::from_secs(3);

impl Restarter {
    fn gave_up(&self) -> bool {
        self.gave_up
    }

    fn sent(&mut self, now: std::time::Instant) {
        self.last_ask = Some(now);
    }

    fn ask_again(&mut self) {
        *self = Self::default();
    }

    /// Whether to send a start request now.
    fn step(
        &mut self,
        state: Option<SessionState>,
        link_ready: bool,
        now: std::time::Instant,
    ) -> bool {
        if self
            .last_ask
            .is_some_and(|at| now.saturating_duration_since(at) < ASK_SETTLE)
        {
            return false;
        }
        let start = match state {
            None | Some(SessionState::Idle) => link_ready && !self.gave_up,
            Some(SessionState::Stopped) if self.gave_up => false,
            Some(SessionState::Stopped) if !link_ready => {
                self.stopped_while_down = true;
                false
            }
            Some(SessionState::Stopped) if self.stopped_while_down => {
                self.stopped_while_down = false;
                true
            }
            // Stopped with the link up: the phone said no, or stopped.
            Some(SessionState::Stopped) => {
                self.gave_up = true;
                false
            }
            _ => false,
        };
        if start {
            self.last_ask = Some(now);
        }
        start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_maps_through_the_letterbox_to_phone_pixels() {
        // 576x1280 video of a 1080x2400 phone, shown in a 1000x1000 widget:
        // scaled to 450x1000, centred with 275 px bars either side.
        let video = (576.0, 1280.0);
        let phone = (1080, 2400);
        let widget = (1000.0, 1000.0);
        assert_eq!(
            map_to_phone((500.0, 500.0), widget, video, phone),
            Some((540, 1200))
        );
        assert_eq!(
            map_to_phone((275.0, 0.0), widget, video, phone),
            Some((0, 0))
        );
        assert_eq!(
            map_to_phone((725.0, 1000.0), widget, video, phone),
            Some((1079, 2399))
        );
        // The black bars are not the phone.
        assert_eq!(map_to_phone((100.0, 500.0), widget, video, phone), None);
        assert_eq!(map_to_phone((900.0, 500.0), widget, video, phone), None);
        // Nothing to map onto yet.
        assert_eq!(
            map_to_phone((500.0, 500.0), widget, (0.0, 0.0), phone),
            None
        );
    }

    #[test]
    fn the_header_shows_time_and_battery_like_the_phones_status_bar() {
        use crate::device_links::device::BatteryStatus;
        let battery = |charge, charging| BatteryStatus {
            current_charge: charge,
            is_charging: charging,
            battery_quantity: None,
            threshold_event: None,
            charge_time_remaining: None,
        };
        assert_eq!(
            phone_status_line(Some(&battery(22, true)), "14:39"),
            "14:39 · Battery 22% ⚡ charging"
        );
        assert_eq!(
            phone_status_line(Some(&battery(80, false)), "14:39"),
            "14:39 · Battery 80%"
        );
        assert_eq!(
            phone_status_line(Some(&battery(-1, false)), "14:39"),
            "14:39"
        );
        assert_eq!(phone_status_line(None, "14:39"), "14:39");
    }

    #[test]
    fn a_press_is_a_tap_a_long_press_or_the_end_of_a_swipe() {
        use std::time::Duration;
        assert_eq!(
            classify_press(false, Duration::from_millis(120)),
            Press::Tap
        );
        assert_eq!(
            classify_press(false, Duration::from_millis(700)),
            Press::Hold
        );
        assert_eq!(
            classify_press(true, Duration::from_millis(120)),
            Press::SwipeEnded
        );
    }

    #[test]
    fn taps_and_swipes_use_the_android_receivers_absolute_fields() {
        let tap = at(action("singleclick"), (10, 20));
        assert_eq!(tap.get("singleclick"), Some(&Value::Bool(true)));
        assert_eq!(
            (tap.get("x"), tap.get("y")),
            (Some(&Value::from(10)), Some(&Value::from(20)))
        );
        // A move during a swipe is just a position.
        let step = at(Map::new(), (5, 6));
        assert_eq!(step.len(), 2);
    }

    /// A refusal or a stop by the phone is asked again by nobody but the
    /// person; a dropped link that comes back is asked again once.
    #[test]
    fn the_phone_screen_never_asks_again_after_a_refusal() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut restarter = Restarter::default();
        restarter.sent(start);
        // The phone declined: Stopped with the link up. Nothing more, ever.
        let mut asked = 0;
        for second in 4..600 {
            let now = start + Duration::from_secs(second);
            if restarter.step(Some(SessionState::Stopped), true, now) {
                asked += 1;
            }
        }
        assert_eq!(asked, 0, "asked again after a refusal");
        assert!(restarter.gave_up());
        // The person asks again.
        restarter.ask_again();
        assert!(!restarter.gave_up());
        restarter.sent(start + Duration::from_secs(700));
        // Still settling: nothing is judged for 3 s.
        assert!(!restarter.step(
            Some(SessionState::Stopped),
            true,
            start + Duration::from_secs(701)
        ));
        assert!(!restarter.gave_up());
    }

    #[test]
    fn a_dropped_link_that_comes_back_is_asked_once() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut restarter = Restarter::default();
        restarter.sent(start);
        // The link drops: the session reads Stopped while down.
        assert!(!restarter.step(
            Some(SessionState::Stopped),
            false,
            start + Duration::from_secs(5)
        ));
        assert!(!restarter.gave_up());
        // It is back: one ask.
        assert!(restarter.step(
            Some(SessionState::Stopped),
            true,
            start + Duration::from_secs(10)
        ));
        // Not again until it settles; then (declined) it gives up.
        assert!(!restarter.step(
            Some(SessionState::Stopped),
            true,
            start + Duration::from_secs(11)
        ));
        assert!(!restarter.step(
            Some(SessionState::Stopped),
            true,
            start + Duration::from_secs(14)
        ));
        assert!(restarter.gave_up());
        // A session that never started asks while the link is up.
        let mut fresh = Restarter::default();
        assert!(fresh.step(None, true, start));
        assert!(!fresh.step(None, true, start + Duration::from_secs(1)));
        assert!(fresh.step(None, true, start + Duration::from_secs(4)));
        assert!(!Restarter::default().step(None, false, start));
    }
}
