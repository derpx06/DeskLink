//! Small drawn widgets shared by the window: the link glyph (this computer,
//! a connector, the phone), the battery ring and the coral presence dot.
//!
//! [`LinkState::of`] decides what the glyph says and has no GTK in it, so it
//! is unit-tested.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::cairo;

use crate::application::token_color;
use crate::device_links::device::{DeviceStatus, DeviceView};

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

    /// "Connected over Wi-Fi": the hero's words.
    pub(crate) fn label(self, device: &DeviceView) -> &'static str {
        match (self, device.connection_path.as_deref()) {
            (Self::Direct, Some("direct")) => "Connected over the internet",
            (Self::Direct, _) => "Connected over Wi-Fi",
            (Self::Relay, _) => "Connected through the relay",
            (Self::Connecting, _) => "Connecting…",
            (Self::Offline, _) => "Offline",
        }
    }

    /// The link's round-trip time ("12 ms") while connected and measured.
    pub(crate) fn round_trip(self, device: &DeviceView) -> Option<String> {
        matches!(self, Self::Direct | Self::Relay)
            .then_some(device.rtt_ms)
            .flatten()
            .map(|ms| format!("{ms} ms"))
    }

    /// "Connected · Wi-Fi": the sidebar's words.
    pub(crate) fn short_label(self, device: &DeviceView) -> &'static str {
        match (self, device.connection_path.as_deref()) {
            (Self::Direct, Some("direct")) => "Connected · Internet",
            (Self::Direct, _) => "Connected · Wi-Fi",
            (Self::Relay, _) => "Connected · Relay",
            (Self::Connecting, _) => "Connecting…",
            (Self::Offline, _) => "Offline",
        }
    }
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(
        x + r,
        y + h - r,
        r,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    cr.arc(
        x + r,
        y + r,
        r,
        std::f64::consts::PI,
        1.5 * std::f64::consts::PI,
    );
    cr.close_path();
}

fn set_colour(cr: &cairo::Context, colour: &gtk::gdk::RGBA, alpha: f64) {
    cr.set_source_rgba(
        f64::from(colour.red()),
        f64::from(colour.green()),
        f64::from(colour.blue()),
        f64::from(colour.alpha()) * alpha,
    );
}

/// The link glyph, 26×11: a computer square, the connector, a phone pill.
/// `on_dark` draws it in white for a dark surface. Named for screen readers
/// with the connection's words.
pub(crate) fn link_glyph(state: LinkState, accessible: &str) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(26)
        .content_height(11)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Start)
        .accessible_role(gtk::AccessibleRole::Img)
        .tooltip_text(accessible)
        .build();
    area.update_property(&[gtk::accessible::Property::Label(accessible)]);
    let pulse = Rc::new(Cell::new(1.0_f64));
    area.set_draw_func({
        let pulse = Rc::clone(&pulse);
        move |area, cr, _, height| {
            let online = matches!(state, LinkState::Direct | LinkState::Relay);
            let fg = area.color();
            let faint = token_color("text_faint");
            let mint = token_color("mint");
            let y = f64::from(height) / 2.0;
            // The computer.
            set_colour(
                cr,
                if online || state == LinkState::Connecting {
                    &fg
                } else {
                    &faint
                },
                1.0,
            );
            rounded(cr, 0.0, y - 4.5, 9.0, 9.0, 2.5);
            let _ = cr.fill();
            // The connector.
            cr.set_line_width(1.5);
            match state {
                LinkState::Direct => {
                    set_colour(cr, &mint, 1.0);
                    cr.rectangle(9.0, y - 0.75, 11.0, 1.5);
                    let _ = cr.fill();
                }
                LinkState::Relay => {
                    set_colour(cr, &mint, 1.0);
                    cr.set_dash(&[2.0, 1.5], 0.0);
                    cr.move_to(9.0, y);
                    cr.line_to(20.0, y);
                    let _ = cr.stroke();
                    cr.set_dash(&[], 0.0);
                }
                LinkState::Connecting => {
                    set_colour(cr, &token_color("accent_bg_color"), pulse.get());
                    cr.rectangle(9.0, y - 0.75, 11.0, 1.5);
                    let _ = cr.fill();
                }
                LinkState::Offline => {
                    set_colour(cr, &faint, 1.0);
                    cr.rectangle(9.0, y - 0.75, 3.85, 1.5);
                    cr.rectangle(16.15, y - 0.75, 3.85, 1.5);
                    let _ = cr.fill();
                }
            }
            // The phone.
            set_colour(cr, if online { &mint } else { &faint }, 1.0);
            rounded(cr, 20.0, y - 5.5, 6.0, 11.0, 2.0);
            let _ = cr.fill();
        }
    });
    // Connecting pulses, unless the user turned animations off.
    if state == LinkState::Connecting
        && gtk::Settings::default().is_none_or(|settings| settings.is_gtk_enable_animations())
    {
        area.add_tick_callback(move |area, clock| {
            let seconds = clock.frame_time() as f64 / 1_000_000.0;
            pulse.set(0.25 + 0.75 * (0.5 + 0.5 * (seconds * std::f64::consts::TAU).sin()));
            area.queue_draw();
            gtk::glib::ControlFlow::Continue
        });
    }
    area
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
        assert_eq!(LinkState::of(&phone).label(&phone), "Connected over Wi-Fi");

        phone.connection_path = Some("direct".into());
        assert_eq!(LinkState::of(&phone), LinkState::Direct);
        assert_eq!(
            LinkState::of(&phone).short_label(&phone),
            "Connected · Internet"
        );

        phone.connection_path = Some("turn".into());
        assert_eq!(LinkState::of(&phone), LinkState::Relay);
        assert_eq!(
            LinkState::of(&phone).short_label(&phone),
            "Connected · Relay"
        );
    }
}
