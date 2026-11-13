//! The phone's calls in the window: a card on the phone's overview, the same
//! card as a bar over every other page, and the ringing and missed-call
//! notifications. Answer, Decline, Silence, Mute and End go to the phone as
//! `desklink.telephony.request`; what it does comes back as its next
//! snapshot.

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use super::ui_kit::{icon_button, icon_square, ButtonKind, Tone};
use super::{can_send_feature, named_widgets, wrapped};
use super::{DesklinkWindow, DeviceView};
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::{CallState, CallStatus, PhoneCall};
use crate::device_links::packet::PACKET_TYPE_TELEPHONY_REQUEST;

/// "0:42", "12:05", "1:02:03".
pub(super) fn timer_words(started_at_ms: i64, now_ms: i64) -> String {
    let seconds = (now_ms - started_at_ms).max(0) / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// What a key does on a call (`ringing`: one is ringing, `talking`: one is
/// up). Enter/A answer, Esc/D decline, S silence, M mute or unmute.
pub(crate) fn shortcut_action(
    key: &str,
    ringing: bool,
    talking: bool,
    muted: bool,
) -> Option<&'static str> {
    match (key.to_lowercase().as_str(), ringing, talking) {
        ("return" | "kp_enter" | "a", true, _) => Some("answer"),
        ("escape" | "d", true, _) => Some("decline"),
        ("s", true, _) => Some("silence"),
        ("m", false, true) => Some(if muted { "unmute" } else { "mute" }),
        _ => None,
    }
}

/// The key hint shown on a button's tooltip.
fn key_hint(action: &str) -> Option<&'static str> {
    match action {
        "answer" => Some("A or Enter"),
        "decline" => Some("D or Esc"),
        "silence" => Some("S"),
        "mute" | "unmute" => Some("M"),
        _ => None,
    }
}

/// Where the sound is, said once after Answer from here: the computer does
/// not carry a call's audio (Android keeps it from apps).
pub(crate) fn audio_hint(answered_here: bool, state: CallState) -> Option<&'static str> {
    (answered_here && state == CallState::Talking).then_some(
        "Talk on your phone. The call's sound stays on the phone. Pick it up, or switch it to speaker.",
    )
}

/// "Call ended, 2:31" for a call that was up; `None` when it never was.
pub(crate) fn ended_summary(
    call: &PhoneCall,
    ready: bool,
    phone: &str,
    now_ms: i64,
) -> Option<String> {
    let started = call
        .started_at
        .filter(|_| call.state == CallState::Talking)?;
    Some(if ready {
        format!("Call ended, {}", timer_words(started, now_ms))
    } else {
        format!("Lost contact with {phone} during the call")
    })
}

/// Whether a ringing call plays the soft sound here.
pub(crate) fn should_ring(ring_setting: bool, shown: bool, banners_shown: Option<bool>) -> bool {
    ring_setting && shown && banners_shown != Some(false)
}

thread_local! {
    /// Calls answered from this computer (shown the audio hint).
    static ANSWERED_HERE: RefCell<std::collections::HashSet<String>> = RefCell::default();
    /// Each phone's last live call, for the "Call ended" summary.
    static LAST_LIVE: RefCell<std::collections::HashMap<String, PhoneCall>> = RefCell::default();
}

fn now_ms() -> i64 {
    glib::real_time() / 1000
}

/// The line under the caller: what the call is doing.
pub(super) fn detail_words(phone: &str, status: &CallStatus, now_ms: i64) -> String {
    let call = status.primary();
    let mut words = match call.state {
        CallState::Ringing if call.incoming => format!("Incoming call on {phone}"),
        CallState::Ringing | CallState::Dialing => format!("Calling from {phone}…"),
        CallState::Held => format!("On hold on {phone}"),
        CallState::Talking => match call.started_at {
            Some(started) => format!("On a call · {}", timer_words(started, now_ms)),
            None => "On a call".to_string(),
        },
    };
    // The name is the headline; the number goes here when both came.
    if let (Some(_), Some(number)) = (&call.contact_name, &call.number) {
        words.push_str(&format!(" · {number}"));
    }
    if status.muted && call.state != CallState::Ringing {
        words.push_str(" · muted");
    }
    if status.silenced && call.state == CallState::Ringing {
        words.push_str(" · silenced");
    }
    words
}

/// What a call button looks like: the kit's button kind, or the answer
/// button's own green (`#0B7A59`, white on it 5.33:1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CallButton {
    Answer,
    Kind(ButtonKind),
}

/// The buttons a call offers now: (action, label, icon, look). The words are
/// exactly Silence, Decline, Answer, Mute, Unmute and End (`UI-REDESIGN.md`
/// 3.18).
pub(crate) fn buttons(
    status: &CallStatus,
) -> Vec<(&'static str, &'static str, &'static str, CallButton)> {
    let call = status.primary();
    if call.incoming && call.state == CallState::Ringing {
        let mut buttons = vec![
            (
                "answer",
                "Answer",
                "desklink-call-symbolic",
                CallButton::Answer,
            ),
            (
                "decline",
                "Decline",
                "desklink-close-symbolic",
                CallButton::Kind(ButtonKind::Danger),
            ),
        ];
        if !status.silenced {
            buttons.push((
                "silence",
                "Silence",
                "desklink-volume-mute-symbolic",
                CallButton::Kind(ButtonKind::Tonal),
            ));
        }
        return buttons;
    }
    vec![
        if status.muted {
            (
                "unmute",
                "Unmute",
                "desklink-mic-symbolic",
                CallButton::Kind(ButtonKind::Tonal),
            )
        } else {
            (
                "mute",
                "Mute",
                "desklink-mic-symbolic",
                CallButton::Kind(ButtonKind::Tonal),
            )
        },
        (
            "end",
            "End",
            "desklink-close-symbolic",
            CallButton::Kind(ButtonKind::Danger),
        ),
    ]
}

fn ringing_here(call: &PhoneCall) -> bool {
    call.incoming && call.state == CallState::Ringing
}

/// The call as a card (`compact`: the bar over other pages).
pub(super) fn card(daemon: &DaemonHandle, device: &DeviceView, compact: bool) -> Option<gtk::Box> {
    if !device.feature_transport_ready || !shown_here() {
        return None;
    }
    let status = device.call_status.as_ref()?;
    let call = status.primary();
    let ringing = ringing_here(call);
    let card = gtk::Box::builder()
        .spacing(12)
        .css_classes(["call-card"])
        .build();
    if compact {
        card.add_css_class("compact");
    } else {
        // On a narrow page the buttons go under the caller.
        card.set_widget_name("responsive");
    }
    if ringing {
        card.add_css_class("ringing");
    }
    let icon = icon_square("desklink-call-symbolic", Tone::Success);
    icon.add_css_class("call-icon");
    // The ring pulses, unless the user turned animations off (GNOME's
    // "Reduce animation" sets this).
    if ringing
        && gtk::Settings::default().is_some_and(|settings| settings.is_gtk_enable_animations())
    {
        icon.add_css_class("pulse");
    }
    card.append(&icon);
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let caller = call.caller();
    // Who is calling and what it is are what the person came for: two lines
    // each before anything is cut (the whole text is then a tooltip).
    let who = wrapped(
        &caller,
        &[if compact { "heading" } else { "dl-card-title" }],
    );
    who.set_lines(2);
    who.set_ellipsize(gtk::pango::EllipsizeMode::End);
    super::tooltip_when_cut(&who);
    words.append(&who);
    let detail = wrapped(
        &detail_words(&device.name, status, now_ms()),
        &["dl-secondary"],
    );
    detail.set_lines(3);
    detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
    super::tooltip_when_cut(&detail);
    if let (CallState::Talking, Some(started)) = (call.state, call.started_at) {
        detail.set_widget_name(&format!("call-timer:{started}"));
    }
    words.append(&detail);
    let answered = ANSWERED_HERE.with(|set| set.borrow().contains(&call.call_id));
    if let Some(hint) = audio_hint(answered, call.state) {
        words.append(&wrapped(hint, &["dl-secondary"]));
    }
    card.append(&words);
    card.update_property(&[gtk::accessible::Property::Label(&format!(
        "{caller}, {}",
        detail_words(&device.name, status, now_ms())
    ))]);
    let can = can_send_feature(device, PACKET_TYPE_TELEPHONY_REQUEST);
    // On a narrow page the buttons wrap under the caller.
    let actions = super::overview::wrap_row(1, 3);
    actions.set_column_spacing(8);
    actions.set_row_spacing(8);
    if !compact {
        actions.set_widget_name("hero-wrap");
    }
    for (action, label, icon, look) in buttons(status) {
        // The bar over other pages stays one line at any width: icons only,
        // their words in the tooltip and for screen readers.
        let button = if compact {
            let button = icon_button(icon, label);
            match look {
                CallButton::Answer => button.add_css_class("call-answer"),
                CallButton::Kind(ButtonKind::Danger) => button.add_css_class("call-danger"),
                CallButton::Kind(_) => {}
            }
            button
        } else {
            match look {
                CallButton::Kind(kind) => super::ui_kit::button(kind, label, Some(icon)),
                CallButton::Answer => {
                    let made = super::ui_kit::button(ButtonKind::Primary, label, Some(icon));
                    made.remove_css_class("dl-btn-primary");
                    made.add_css_class("call-answer");
                    made
                }
            }
        };
        button.set_sensitive(can);
        if !can {
            button.set_tooltip_text(Some("Update DeskLink on the phone to answer here"));
        }
        if let Some(keys) = key_hint(action) {
            if can {
                button.set_tooltip_text(Some(&format!("{label} ({keys})")));
            }
            button.update_property(&[gtk::accessible::Property::KeyShortcuts(keys)]);
        }
        let (daemon, id) = (daemon.clone(), device.id.clone());
        let call_id = call.call_id.clone();
        button.connect_clicked(move |_| {
            if action == "answer" {
                ANSWERED_HERE.with(|set| set.borrow_mut().insert(call_id.clone()));
            }
            daemon.send(DaemonCommand::Telephony(
                id.clone(),
                action.to_string(),
                None,
            ))
        });
        super::overview::add_wrapped(&actions, &button);
    }
    card.append(&actions);
    Some(card)
}

/// The phone with a call to show: one ringing first.
/// "Show calls on this computer" (Settings, Calls): off hides the card, the
/// bar and the ring notification, and the phone just rings.
fn shown_here() -> bool {
    crate::device_links::config::app_settings().calls_on_computer
}

fn calling(devices: &[DeviceView]) -> Option<&DeviceView> {
    if !shown_here() {
        return None;
    }
    let live = || {
        devices
            .iter()
            .filter(|device| device.trusted && device.feature_transport_ready)
            .filter(|device| device.call_status.is_some())
    };
    live()
        .find(|device| {
            device
                .call_status
                .as_ref()
                .is_some_and(|status| status.ringing().is_some())
        })
        .or_else(|| live().next())
}

/// The bar over every page but that phone's own overview (which has the
/// card), redrawn when the call changes; its timer ticks in place.
pub(super) fn update_bar(win: &DesklinkWindow, daemon: &DaemonHandle, devices: &[DeviceView]) {
    announce_ends(win, devices);
    let imp = win.imp();
    let on_overview = |device: &DeviceView| {
        imp.page.borrow().as_str() == "home"
            && imp.selected_device_id.borrow().as_deref() == Some(device.id.as_str())
    };
    let shown = calling(devices).filter(|device| !on_overview(device));
    let key = shown
        .map(|device| serde_json::to_string(&(&device.id, &device.call_status)).unwrap_or_default())
        .unwrap_or_default();
    if *imp.call_bar_key.borrow() != key {
        imp.call_bar_key.replace(key);
        match shown.and_then(|device| card(daemon, device, true)) {
            Some(bar) => {
                bar.set_margin_start(12);
                bar.set_margin_end(12);
                bar.set_margin_top(6);
                bar.set_margin_bottom(6);
                imp.call_bar.set_child(Some(&bar));
                imp.call_bar.set_reveal_child(true);
            }
            None => imp.call_bar.set_reveal_child(false),
        }
    }
    tick(&imp.call_bar.clone().upcast());
    if let Some((_, body)) = imp.bodies.borrow().get("home").cloned() {
        tick(&body.upcast());
    }
}

/// Moves every call timer under `root` to now.
fn tick(root: &gtk::Widget) {
    for widget in named_widgets(root, "call-timer:") {
        let Some(label) = widget.downcast_ref::<gtk::Label>() else {
            continue;
        };
        let Ok(started) = widget
            .widget_name()
            .trim_start_matches("call-timer:")
            .parse::<i64>()
        else {
            continue;
        };
        let text = label.label();
        // "On a call · 1:05 · …": only the time moves.
        let mut parts: Vec<String> = text.split(" · ").map(str::to_string).collect();
        if parts.len() >= 2 {
            parts[1] = timer_words(started, now_ms());
            let updated = parts.join(" · ");
            if updated != text {
                label.set_label(&updated);
            }
        }
    }
}

/// One soft chime from the sound theme, never blocking; silent where no
/// player exists.
fn ring_sound() {
    let _ = std::process::Command::new("canberra-gtk-play")
        .args(["-i", "phone-incoming-call", "-V", "-6"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|mut child| std::thread::spawn(move || drop(child.wait())));
}

/// Says how each call ended (its length, or that contact was lost).
fn announce_ends(win: &DesklinkWindow, devices: &[DeviceView]) {
    for device in devices.iter().filter(|device| device.trusted) {
        let live = device
            .call_status
            .as_ref()
            .map(CallStatus::primary)
            .filter(|call| call.state == CallState::Talking && call.started_at.is_some())
            .filter(|_| device.feature_transport_ready);
        if let Some(call) = live {
            LAST_LIVE.with(|last| last.borrow_mut().insert(device.id.clone(), call.clone()));
        } else if device.call_status.is_none() || !device.feature_transport_ready {
            if let Some(call) = LAST_LIVE.with(|last| last.borrow_mut().remove(&device.id)) {
                ANSWERED_HERE.with(|set| set.borrow_mut().remove(&call.call_id));
                if let Some(text) = ended_summary(
                    &call,
                    device.feature_transport_ready,
                    &device.name,
                    now_ms(),
                ) {
                    win.show_feature_toast(&text);
                }
            }
        }
    }
}

/// Keys on the whole window act on a ringing or live call, unless the
/// person is typing.
pub(super) fn install_shortcuts(win: &DesklinkWindow) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = win.downgrade();
    keys.connect_key_pressed(move |_, key, _, state| {
        let Some(win) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        if state.intersects(
            gtk::gdk::ModifierType::CONTROL_MASK
                | gtk::gdk::ModifierType::ALT_MASK
                | gtk::gdk::ModifierType::SUPER_MASK,
        ) || gtk::prelude::GtkWindowExt::focus(&win).is_some_and(|focus| {
            focus.is::<gtk::Editable>() || focus.ancestor(gtk::Text::static_type()).is_some()
        }) {
            return glib::Propagation::Proceed;
        }
        let Some(name) = key.name() else {
            return glib::Propagation::Proceed;
        };
        let (Some(daemon), devices) = (win.daemon(), win.paired_devices()) else {
            return glib::Propagation::Proceed;
        };
        let Some(device) = calling(&devices) else {
            return glib::Propagation::Proceed;
        };
        let Some(status) = device.call_status.as_ref() else {
            return glib::Propagation::Proceed;
        };
        let talking = status.primary().state == CallState::Talking;
        match shortcut_action(&name, status.ringing().is_some(), talking, status.muted) {
            Some(action) if can_send_feature(device, PACKET_TYPE_TELEPHONY_REQUEST) => {
                if action == "answer" {
                    if let Some(call) = status.ringing() {
                        ANSWERED_HERE.with(|set| set.borrow_mut().insert(call.call_id.clone()));
                    }
                }
                daemon.send(DaemonCommand::Telephony(
                    device.id.clone(),
                    action.to_string(),
                    None,
                ));
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    win.add_controller(keys);
}

/// A notification for each call ringing here — unless the top-bar
/// extension shows its own banner — taken back once it stops ringing.
pub(super) fn notify(win: &DesklinkWindow, devices: &[DeviceView]) {
    let Some(app) = win.application() else {
        return;
    };
    let imp = win.imp();
    let shown = shown_here();
    let ringing: Vec<(&DeviceView, &PhoneCall)> = devices
        .iter()
        .filter(|device| shown && device.trusted && device.feature_transport_ready)
        .filter_map(|device| {
            let call = device.call_status.as_ref()?.ringing()?;
            Some((device, call))
        })
        .collect();
    // Keyed by what it says too: the caller's name often arrives a moment
    // after the ring, and the notification must then say who it is.
    let key = |device: &DeviceView, call: &PhoneCall| {
        format!(
            "{}:{}:{}:{}",
            device.id,
            call.call_id,
            call.caller(),
            call.number.as_deref().unwrap_or_default()
        )
    };
    let keys: std::collections::HashSet<String> = ringing
        .iter()
        .map(|(device, call)| key(device, call))
        .collect();
    let ended: Vec<String> = imp
        .notified_calls
        .borrow()
        .iter()
        .filter(|key| !keys.contains(*key))
        .cloned()
        .collect();
    for key in ended {
        imp.notified_calls.borrow_mut().remove(&key);
        if let Some((device, _)) = key.split_once(':') {
            app.withdraw_notification(&format!("call-{device}"));
        }
    }
    let banner_elsewhere = crate::shell_dbus::client_handles("calls");
    for (device, call) in ringing {
        if !imp.notified_calls.borrow_mut().insert(key(device, call)) || banner_elsewhere {
            continue;
        }
        // Locked: the name and number stay off the lock screen unless allowed.
        let (title, body) = crate::device_links::calls::ring_text(
            &call.caller(),
            call.number.as_deref(),
            call.contact_name.is_some(),
            &device.name,
            crate::device_links::logind::locked().unwrap_or(false),
            crate::device_links::config::app_settings().caller_on_lock_screen,
        );
        let notification = gio::Notification::new(&title);
        notification.set_body(Some(&body));
        notification.set_priority(gio::NotificationPriority::Urgent);
        notification.set_icon(&gio::ThemedIcon::new("call-start-symbolic"));
        if can_send_feature(device, PACKET_TYPE_TELEPHONY_REQUEST) {
            for (action, label) in [
                ("answer", "Answer"),
                ("decline", "Decline"),
                ("silence", "Silence"),
            ] {
                notification.add_button_with_target_value(
                    label,
                    "app.call",
                    Some(&(device.id.as_str(), action, "").to_variant()),
                );
            }
        }
        app.send_notification(Some(&format!("call-{}", device.id)), &notification);
        let banners = crate::device_links::dnd::GnomeBanners::open()
            .and_then(|banners| crate::device_links::dnd::Banners::shown(&banners));
        if should_ring(
            crate::device_links::config::app_settings().ring_on_computer,
            shown,
            banners,
        ) {
            ring_sound();
        }
    }
}

/// "Missed call from Alice", with Call Back when the number came.
pub(super) fn missed(
    win: &DesklinkWindow,
    device_id: &str,
    device_name: &str,
    caller: Option<String>,
    number: Option<String>,
) {
    let Some(app) = win.application() else {
        return;
    };
    let settings = crate::device_links::config::app_settings();
    if !settings.calls_on_computer {
        return;
    }
    let (caller, number) = if crate::device_links::logind::locked().unwrap_or(false)
        && !settings.caller_on_lock_screen
    {
        (None, None)
    } else {
        (caller, number)
    };
    let title = match caller.as_ref().or(number.as_ref()) {
        Some(who) => format!("Missed call from {who}"),
        None => "Missed call".to_string(),
    };
    let notification = gio::Notification::new(&title);
    let mut body = format!("On {device_name}");
    if let (Some(_), Some(number)) = (&caller, &number) {
        body.push_str(&format!(" · {number}"));
    }
    notification.set_body(Some(&body));
    notification.set_icon(&gio::ThemedIcon::new("call-missed-symbolic"));
    if let Some(number) = &number {
        notification.add_button_with_target_value(
            "Call Back",
            "app.call",
            Some(&(device_id, "dial", number.as_str()).to_variant()),
        );
    }
    app.send_notification(
        Some(&format!(
            "missed-call-{device_id}-{}",
            glib::monotonic_time()
        )),
        &notification,
    );
}

/// Whether a call confirmation may open now: not while one is open, and not
/// within [`DIAL_ASK_EVERY`] of the last (a web page looping `tel:` links
/// must not stack dialogs or keep the window raised).
fn may_ask_to_dial(open: bool, last: Option<Instant>, now: Instant) -> bool {
    !open && last.is_none_or(|last| now.duration_since(last) >= DIAL_ASK_EVERY)
}

const DIAL_ASK_EVERY: Duration = Duration::from_secs(3);

/// What becomes of a request to ask about `number`.
#[derive(Debug, PartialEq, Eq)]
enum Admit {
    Ask(String),
    NotANumber,
    /// An emergency number: never from here.
    Refused(&'static str),
    Throttled,
    /// A question is on screen already.
    Open,
}

/// The number is checked FIRST, so a request that is not a number costs
/// nothing and cannot lock out a real one; only an accepted request counts.
fn admit(number: &str, open: bool, last: Option<Instant>, now: Instant) -> Admit {
    let Some(number) = crate::device_links::calls::dial_number(number) else {
        return Admit::NotANumber;
    };
    if let Some(why) = crate::device_links::calls::dial_refusal(&number) {
        return Admit::Refused(why);
    }
    if open {
        Admit::Open
    } else if !may_ask_to_dial(open, last, now) {
        Admit::Throttled
    } else {
        Admit::Ask(number)
    }
}

thread_local! {
    static DIAL_ASKED: Cell<Option<Instant>> = const { Cell::new(None) };
    static DIAL_DIALOG: RefCell<Option<glib::WeakRef<adw::AlertDialog>>> = const { RefCell::new(None) };
}

/// Asks before calling `number` from a link (`tel:`), `desklink --dial`, the
/// search provider, a missed-call button or a contact: a web page, or any
/// process on the session bus, can start a call request without the user
/// meaning to. Every dial goes through here and nowhere else. The default
/// answer is Cancel, so a stray Enter never places a call. With `phone`, that
/// phone places the call; else the first that can.
pub(crate) fn confirm_dial(win: &DesklinkWindow, number: &str, phone: Option<&str>) {
    confirm_telephony(win, number, phone, "dial");
}

/// The same question for a text message: the phone opens its composer for
/// `number`, so a session-bus caller cannot do it unasked either.
pub(crate) fn confirm_message(win: &DesklinkWindow, number: &str, phone: Option<&str>) {
    confirm_telephony(win, number, phone, "message");
}

/// What a throttled request is told.
pub(crate) const TOO_MANY_CALLS: &str = "Too many calls: wait a few seconds";

fn confirm_telephony(
    win: &DesklinkWindow,
    number: &str,
    phone: Option<&str>,
    action: &'static str,
) {
    let Some(daemon) = win.imp().daemon.borrow().clone() else {
        return;
    };
    let open = DIAL_DIALOG.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(glib::WeakRef::upgrade)
            .is_some_and(|dialog| dialog.is_mapped())
    });
    let now = Instant::now();
    let number = match admit(number, open, DIAL_ASKED.with(Cell::get), now) {
        Admit::Ask(number) => number,
        Admit::NotANumber => {
            win.show_feature_toast("That is not a phone number");
            return;
        }
        Admit::Refused(why) => {
            win.show_feature_toast(why);
            return;
        }
        Admit::Throttled => {
            win.show_feature_toast(TOO_MANY_CALLS);
            return;
        }
        Admit::Open => return,
    };
    // Only a request that will be asked uses up the allowance.
    DIAL_ASKED.with(|last| last.set(Some(now)));
    let phone = daemon
        .devices()
        .into_iter()
        .filter(|device| device.trusted && device.feature_transport_ready)
        .filter(|device| phone.is_none_or(|id| id == device.id))
        .find(|device| can_send_feature(device, PACKET_TYPE_TELEPHONY_REQUEST));
    let Some(phone) = phone else {
        let dialog = adw::AlertDialog::new(
            Some("No Phone to Call With"),
            Some("Connect a phone with DeskLink to make calls from this computer."),
        );
        dialog.add_response("close", "_Close");
        dialog.present(Some(win));
        return;
    };
    let (title, body, button) = if action == "message" {
        (
            format!("Message {number}?"),
            format!("{} opens a message to this number.", phone.name),
            "_Message",
        )
    } else {
        let warning = if crate::device_links::calls::looks_premium(&number) {
            " This looks like a premium-rate number and may cost extra."
        } else {
            ""
        };
        (
            format!("Call {number}?"),
            format!("{} places the call.{warning}", phone.name),
            "_Call",
        )
    };
    let dialog = adw::AlertDialog::new(Some(&title), Some(&body));
    dialog.add_response("cancel", "_Cancel");
    dialog.add_response("go", button);
    dialog.set_response_appearance("go", adw::ResponseAppearance::Suggested);
    // Cancel is the default: Enter or Space never places a call.
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    DIAL_DIALOG.with(|slot| *slot.borrow_mut() = Some(dialog.downgrade()));
    dialog.connect_response(Some("go"), move |_, _| {
        if action == "dial" {
            crate::device_links::dial_recents::add(&number);
        }
        daemon.send(DaemonCommand::Telephony(
            phone.id.clone(),
            action.to_string(),
            Some(number.clone()),
        ));
    });
    dialog.present(Some(win));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: CallState, incoming: bool, muted: bool) -> CallStatus {
        CallStatus {
            calls: vec![PhoneCall {
                call_id: "1".into(),
                state,
                contact_name: Some("Alice".into()),
                number: Some("+1 555 0100".into()),
                started_at: Some(1_000),
                incoming,
            }],
            muted,
            silenced: false,
        }
    }

    #[test]
    fn a_junk_request_never_uses_up_the_allowance_and_a_throttled_one_says_so() {
        let now = Instant::now();
        assert_eq!(admit("evil;rm", false, None, now), Admit::NotANumber);
        // Junk right after a real ask is still "not a number", not "throttled".
        assert_eq!(
            admit("+1 555 0100", false, Some(now), now),
            Admit::Throttled
        );
        assert_eq!(admit("junk", false, Some(now), now), Admit::NotANumber);
        assert_eq!(
            admit("+1 555 0100", false, None, now),
            Admit::Ask("+15550100".into())
        );
        assert_eq!(
            admit("+1 555 0100", false, Some(now), now + DIAL_ASK_EVERY),
            Admit::Ask("+15550100".into())
        );
        assert_eq!(admit("+1 555 0100", true, None, now), Admit::Open);
        assert!(TOO_MANY_CALLS.starts_with("Too many calls"));
    }

    #[test]
    fn the_timer_reads_minutes_then_hours() {
        assert_eq!(timer_words(1_000, 1_000), "0:00");
        assert_eq!(timer_words(0, 65_000), "1:05");
        assert_eq!(timer_words(0, 3_723_000), "1:02:03");
        assert_eq!(
            timer_words(5_000, 1_000),
            "0:00",
            "a clock skew never goes negative"
        );
    }

    #[test]
    fn a_ringing_call_offers_answer_and_a_live_one_mute_and_end() {
        let actions = |status: &CallStatus| -> Vec<&str> {
            buttons(status)
                .into_iter()
                .map(|(action, ..)| action)
                .collect()
        };
        assert_eq!(
            actions(&status(CallState::Ringing, true, false)),
            ["answer", "decline", "silence"]
        );
        assert_eq!(
            actions(&status(CallState::Talking, true, false)),
            ["mute", "end"]
        );
        assert_eq!(
            actions(&status(CallState::Talking, true, true)),
            ["unmute", "end"]
        );
        assert_eq!(
            actions(&status(CallState::Ringing, false, false)),
            ["mute", "end"],
            "an outgoing call ringing the other end is not answered here"
        );
        let mut silenced = status(CallState::Ringing, true, false);
        silenced.silenced = true;
        assert_eq!(actions(&silenced), ["answer", "decline"]);
    }

    #[test]
    fn the_detail_line_says_what_the_call_is_doing() {
        assert_eq!(
            detail_words("Pixel", &status(CallState::Ringing, true, false), 0),
            "Incoming call on Pixel · +1 555 0100"
        );
        assert_eq!(
            detail_words("Pixel", &status(CallState::Talking, true, true), 66_000),
            "On a call · 1:05 · +1 555 0100 · muted"
        );
        assert_eq!(
            detail_words("Pixel", &status(CallState::Dialing, false, false), 0),
            "Calling from Pixel… · +1 555 0100"
        );
    }

    /// A call confirmation does not open over another, nor again within three
    /// seconds of the last: a page looping `tel:` links cannot stack dialogs.
    #[test]
    fn a_call_confirmation_never_stacks_and_is_rate_limited() {
        let now = Instant::now();
        assert!(may_ask_to_dial(false, None, now));
        assert!(!may_ask_to_dial(true, None, now), "one at a time");
        assert!(!may_ask_to_dial(
            false,
            Some(now),
            now + Duration::from_secs(1)
        ));
        assert!(may_ask_to_dial(false, Some(now), now + DIAL_ASK_EVERY));
    }

    /// Nothing but the confirmation starts a dial: the only production files
    /// that mention both the `"dial"` verb and a telephony command are this
    /// one (which asks first) and `application.rs`, whose `app.call` hands a
    /// dial to the same confirmation (`call_action_kind`).
    #[test]
    fn only_the_confirmation_dialog_places_a_call() {
        fn scan(dir: &std::path::Path, found: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    scan(&path, found);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    // Production code only: stop at the test module.
                    let text = text.split("#[cfg(test)]").next().unwrap_or_default();
                    if text.contains("\"dial\"") && text.contains("Telephony(") {
                        found.push(path.display().to_string());
                    }
                }
            }
        }
        let mut found = Vec::new();
        scan(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut found,
        );
        found.sort();
        assert!(
            found.iter().any(|path| path.ends_with("window/calls.rs")),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .all(|path| path.ends_with("window/calls.rs")
                    || path.ends_with("src/application.rs")),
            "{found:?}"
        );
    }

    #[test]
    fn keys_map_to_the_call_actions() {
        assert_eq!(
            shortcut_action("Return", true, false, false),
            Some("answer")
        );
        assert_eq!(shortcut_action("a", true, false, false), Some("answer"));
        assert_eq!(
            shortcut_action("Escape", true, false, false),
            Some("decline")
        );
        assert_eq!(shortcut_action("D", true, false, false), Some("decline"));
        assert_eq!(shortcut_action("s", true, false, false), Some("silence"));
        assert_eq!(shortcut_action("m", false, true, false), Some("mute"));
        assert_eq!(shortcut_action("m", false, true, true), Some("unmute"));
        // Nothing ringing: Enter and Esc leave the call alone.
        assert_eq!(shortcut_action("Return", false, true, false), None);
        assert_eq!(shortcut_action("Escape", false, true, false), None);
        assert_eq!(shortcut_action("a", false, false, false), None);
    }

    #[test]
    fn the_audio_hint_shows_only_for_a_call_answered_here() {
        assert!(audio_hint(true, CallState::Talking)
            .is_some_and(|h| h.starts_with("Talk on your phone")));
        assert_eq!(audio_hint(false, CallState::Talking), None);
        assert_eq!(audio_hint(true, CallState::Ringing), None);
    }

    #[test]
    fn a_call_that_was_up_ends_with_its_length_or_lost_contact() {
        let mut call = PhoneCall {
            call_id: "1".into(),
            state: CallState::Talking,
            contact_name: None,
            number: None,
            started_at: Some(1_000),
            incoming: true,
        };
        assert_eq!(
            ended_summary(&call, true, "Pixel", 1_000 + 151_000).as_deref(),
            Some("Call ended, 2:31")
        );
        assert_eq!(
            ended_summary(&call, false, "Pixel", 5_000).as_deref(),
            Some("Lost contact with Pixel during the call")
        );
        call.state = CallState::Ringing;
        assert_eq!(ended_summary(&call, true, "Pixel", 5_000), None);
    }

    #[test]
    fn the_ring_is_quiet_in_do_not_disturb_and_when_off() {
        assert!(should_ring(true, true, Some(true)));
        assert!(should_ring(true, true, None), "no such setting: not DND");
        assert!(!should_ring(true, true, Some(false)));
        assert!(!should_ring(false, true, Some(true)));
        assert!(!should_ring(true, false, Some(true)));
    }

    #[test]
    fn the_call_bar_has_labelled_buttons_with_key_hints() {
        crate::test_display::run(|| {
            let (daemon, _commands) = DaemonHandle::detached();
            let info =
                crate::device_links::device_info::DeviceInfo::local("p".into(), "Pixel".into());
            let mut device = DeviceView::from_info(&info, "192.168.1.9".into(), true);
            device.trusted = true;
            device.feature_transport_ready = true;
            device.incoming_capabilities = vec![PACKET_TYPE_TELEPHONY_REQUEST.to_string()];
            device.call_status = Some(status(CallState::Ringing, true, false));
            let bar = card(&daemon, &device, true).expect("a card for a ringing call");
            let mut tips = Vec::new();
            fn walk(widget: &gtk::Widget, tips: &mut Vec<String>) {
                if let Some(tip) = widget.tooltip_text() {
                    tips.push(tip.to_string());
                }
                let mut child = widget.first_child();
                while let Some(c) = child {
                    walk(&c, tips);
                    child = c.next_sibling();
                }
            }
            walk(bar.upcast_ref(), &mut tips);
            for want in ["Answer (A or Enter)", "Decline (D or Esc)", "Silence (S)"] {
                assert!(tips.iter().any(|t| t == want), "{want} in {tips:?}");
            }
        });
    }
}
