//! Small drawn widgets shared by the window: the battery ring and the coral
//! presence dot, and the words for a device's state ([`Presentation`]).
//!
//! [`LinkState::of`] decides what the glyph says and has no GTK in it, so it
//! is unit-tested.

use adw::prelude::*;
use gtk::cairo;

use crate::application::token_color;
use crate::device_links::device::{DeviceStatus, DeviceView};
use crate::window::ui_kit::Status;

/// How a device is linked to this computer, as the glyph's connector shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkState {
    /// Solid: over the local network, or straight over the internet.
    Direct,
    /// Dashed: through a TURN relay.
    Relay,
    /// Pulsing: reachable, the encrypted connection still being set up.
    Connecting,
    /// Broken: not reachable.
    Offline,
}

impl LinkState {
    pub(crate) fn of(device: &DeviceView) -> Self {
        match device.status {
            DeviceStatus::Paired if !device.feature_transport_ready => Self::Connecting,
            DeviceStatus::Paired => match device.connection_path.as_deref() {
                Some("turn") => Self::Relay,
                _ => Self::Direct,
            },
            DeviceStatus::Unreachable | DeviceStatus::Discovered => Self::Offline,
            _ => Self::Connecting,
        }
    }

    /// The link's round-trip time ("12 ms") while connected and measured.
    pub(crate) fn round_trip(self, device: &DeviceView) -> Option<String> {
        matches!(self, Self::Direct | Self::Relay)
            .then_some(device.rtt_ms)
            .flatten()
            .map(|ms| format!("{ms} ms"))
    }
}

/// What a device's state is called, in the words every surface shares (phone,
/// desk, top bar; `UI-REDESIGN.md` 3.0): a pill word, a longer detail line,
/// and a short detail for narrow places.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Presentation {
    pub(crate) status: Status,
    /// The pill: "Connected", "Connecting…", "Can't reach", "Needs your OK".
    pub(crate) word: &'static str,
    /// "over Wi-Fi · 9 ms", "Check that Pixel 9 is on and on your network".
    pub(crate) detail: String,
    /// "Wi-Fi · 9 ms": the same for a place that is 200 px wide.
    pub(crate) short: String,
}

impl Presentation {
    pub(crate) fn of(device: &DeviceView) -> Self {
        let make = |status, word, detail: &str, short: &str| Self {
            status,
            word,
            detail: detail.to_string(),
            short: short.to_string(),
        };
        if !device.trusted {
            return match device.status {
                DeviceStatus::PairRequestedByPeer => make(
                    Status::Warn,
                    "Needs your OK",
                    "Wants to pair",
                    "Wants to pair",
                ),
                DeviceStatus::PairRequested => make(
                    Status::Idle,
                    "Waiting…",
                    "Accept the request on the phone",
                    "Accept on the phone",
                ),
                DeviceStatus::Discovered | DeviceStatus::Connected => {
                    make(Status::Idle, "Nearby", "Not paired yet", "Not paired yet")
                }
                _ => make(Status::Idle, "Connecting…", "", ""),
            };
        }
        let state = LinkState::of(device);
        match state {
            LinkState::Direct | LinkState::Relay => {
                // The internet, direct or through the relay, reads the same:
                // end to end encrypted either way.
                let (route, short_route) = match (state, device.connection_path.as_deref()) {
                    (LinkState::Relay, _) | (_, Some("direct")) => {
                        ("over the internet · encrypted", "Internet")
                    }
                    _ => ("over Wi-Fi", "Wi-Fi"),
                };
                let rtt = state.round_trip(device);
                let with_rtt = |text: &str| match &rtt {
                    Some(rtt) => format!("{text} · {rtt}"),
                    None => text.to_string(),
                };
                Self {
                    status: Status::Ok,
                    word: "Connected",
                    detail: with_rtt(route),
                    short: with_rtt(short_route),
                }
            }
            LinkState::Connecting => make(
                Status::Idle,
                "Connecting…",
                "Setting up a secure connection",
                "Setting up",
            ),
            LinkState::Offline => Self {
                status: Status::Err,
                word: "Can't reach",
                detail: format!("Check that {} is on and on your network", device.name),
                short: "Not on your network".to_string(),
            },
        }
    }

    /// "Connected over Wi-Fi · 9 ms": a screen reader's and a tooltip's words.
    pub(crate) fn spoken(&self) -> String {
        if self.detail.is_empty() {
            self.word.to_string()
        } else {
            format!("{} {}", self.word, self.detail)
        }
    }
}

fn set_colour(cr: &cairo::Context, colour: &gtk::gdk::RGBA, alpha: f64) {
    cr.set_source_rgba(
        f64::from(colour.red()),
        f64::from(colour.green()),
        f64::from(colour.blue()),
        f64::from(colour.alpha()) * alpha,
    );
}

/// The battery as a 28 px ring, mint for the charge, with a bolt while
/// charging.
pub(crate) fn battery_ring(percent: i64, charging: bool) -> gtk::DrawingArea {
    let label = if charging {
        format!("Battery {percent}%, charging")
    } else {
        format!("Battery {percent}%")
    };
    let area = gtk::DrawingArea::builder()
        .content_width(28)
        .content_height(28)
        .valign(gtk::Align::Center)
        .accessible_role(gtk::AccessibleRole::Img)
        .build();
    area.update_property(&[gtk::accessible::Property::Label(&label)]);
    let fraction = (percent.clamp(0, 100) as f64) / 100.0;
    area.set_draw_func(move |_, cr, width, height| {
        let (cx, cy) = (f64::from(width) / 2.0, f64::from(height) / 2.0);
        let radius = cx.min(cy) - 1.5;
        let start = -std::f64::consts::FRAC_PI_2;
        cr.set_line_width(3.0);
        set_colour(cr, &token_color("surface_2"), 1.0);
        cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
        let _ = cr.stroke();
        let mint = token_color("mint");
        set_colour(cr, &mint, 1.0);
        cr.arc(
            cx,
            cy,
            radius,
            start,
            start + fraction * std::f64::consts::TAU,
        );
        let _ = cr.stroke();
        if charging {
            // A bolt, 10 px tall, centred.
            cr.move_to(cx + 1.0, cy - 5.5);
            cr.line_to(cx - 3.0, cy + 1.0);
            cr.line_to(cx - 0.2, cy + 1.0);
            cr.line_to(cx - 1.0, cy + 5.5);
            cr.line_to(cx + 3.0, cy - 1.0);
            cr.line_to(cx + 0.2, cy - 1.0);
            cr.close_path();
            let _ = cr.fill();
        }
    });
    area
}

/// The 6 px coral dot: another device is using this right now.
pub(crate) fn presence_dot(accessible: &str) -> gtk::Box {
    let dot = gtk::Box::builder()
        .valign(gtk::Align::Center)
        .css_classes(["dot", "coral"])
        .accessible_role(gtk::AccessibleRole::Img)
        .tooltip_text(accessible)
        .build();
    dot.update_property(&[gtk::accessible::Property::Label(accessible)]);
    dot
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::device_info::DeviceInfo;

    fn device() -> DeviceView {
        let info = DeviceInfo {
            id: "0123456789abcdef0123456789abcdef".to_string(),
            name: "Pixel".to_string(),
            device_type: "phone".to_string(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
            app_version: None,
        };
        DeviceView::from_info(&info, String::new(), true)
    }

    #[test]
    fn the_connector_shows_how_the_phone_is_reached() {
        let mut phone = device();
        phone.status = DeviceStatus::Unreachable;
        assert_eq!(LinkState::of(&phone), LinkState::Offline);

        phone.status = DeviceStatus::Paired;
        assert_eq!(
            LinkState::of(&phone),
            LinkState::Connecting,
            "handover not done"
        );

        phone.feature_transport_ready = true;
        phone.connection_path = Some("lan".into());
        assert_eq!(LinkState::of(&phone), LinkState::Direct);
        let words = Presentation::of(&phone);
        assert_eq!((words.status, words.word), (Status::Ok, "Connected"));
        assert_eq!(words.detail, "over Wi-Fi");
        phone.rtt_ms = Some(9);
        assert_eq!(Presentation::of(&phone).detail, "over Wi-Fi · 9 ms");
        assert_eq!(Presentation::of(&phone).short, "Wi-Fi · 9 ms");
        assert_eq!(
            Presentation::of(&phone).spoken(),
            "Connected over Wi-Fi · 9 ms"
        );

        // The internet reads the same direct or through the relay, and the
        // words never say "relay".
        for path in ["direct", "turn"] {
            phone.connection_path = Some(path.into());
            let words = Presentation::of(&phone);
            assert_eq!(words.detail, "over the internet · encrypted · 9 ms");
            assert!(!words.detail.to_lowercase().contains("relay"));
        }
        phone.connection_path = Some("turn".into());
        assert_eq!(LinkState::of(&phone), LinkState::Relay);
    }

    /// The same words as the phone's and the shell menu's (3.0): one word per
    /// state, a shape per word, and never "Connected" for a link that is not
    /// feature-ready.
    #[test]
    fn every_state_has_its_shared_word_and_unreachable_is_never_connected() {
        let mut phone = device();
        phone.status = DeviceStatus::Paired;
        let connecting = Presentation::of(&phone);
        assert_eq!(
            (connecting.status, connecting.word),
            (Status::Idle, "Connecting…")
        );

        phone.status = DeviceStatus::Unreachable;
        let away = Presentation::of(&phone);
        assert_eq!((away.status, away.word), (Status::Err, "Can't reach"));
        assert_eq!(away.detail, "Check that Pixel is on and on your network");

        phone.trusted = false;
        phone.status = DeviceStatus::PairRequestedByPeer;
        let asks = Presentation::of(&phone);
        assert_eq!((asks.status, asks.word), (Status::Warn, "Needs your OK"));
        phone.status = DeviceStatus::Discovered;
        assert_eq!(Presentation::of(&phone).word, "Nearby");
        phone.status = DeviceStatus::PairRequested;
        assert_eq!(Presentation::of(&phone).status, Status::Idle);
    }
}
