//! The popup's picture: a laptop and a phone joined by a link, with an arrow
//! badge pointing up. Drawn on the spot from the style's own colours, so it
//! follows light, dark and high contrast (high contrast draws outlines in the
//! text colour, no tints) and needs no image file. Decorative.
//!
//! Sibling edges: none. Outside: `application::token_color`.

use gtk::prelude::*;

use crate::application::token_color;

const WIDTH: f64 = 300.0;
const HEIGHT: f64 = 130.0;

fn colour(cr: &gtk::cairo::Context, name: &str, alpha: f64) {
    let c = token_color(name);
    cr.set_source_rgba(
        f64::from(c.red()),
        f64::from(c.green()),
        f64::from(c.blue()),
        f64::from(c.alpha()) * alpha,
    );
}

fn rounded(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let pi = std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -pi / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, pi / 2.0);
    cr.arc(x + r, y + h - r, r, pi / 2.0, pi);
    cr.arc(x + r, y + r, r, pi, 3.0 * pi / 2.0);
    cr.close_path();
}

/// A 300 x 130 drawing, scaled to the width it is given.
pub(super) fn illustration(warning: bool) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(WIDTH as i32)
        .content_height(HEIGHT as i32)
        .halign(gtk::Align::Center)
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    area.set_draw_func(move |_, cr, width, height| {
        let high = adw::StyleManager::default().is_high_contrast();
        let scale = (f64::from(width) / WIDTH).min(f64::from(height) / HEIGHT);
        cr.translate(
            (f64::from(width) - WIDTH * scale) / 2.0,
            (f64::from(height) - HEIGHT * scale) / 2.0,
        );
        cr.scale(scale, scale);
        let main = if warning { "dl_warn" } else { "dl_accent" };
        let soft = if warning {
            "dl_warn_container"
        } else {
            "dl_accent_container"
        };
        // The soft backdrop.
        if !high {
            colour(cr, soft, 1.0);
            rounded(cr, 0.0, 0.0, WIDTH, HEIGHT, 20.0);
            let _ = cr.fill();
        }
        let line = if high { "dl_text" } else { main };
        cr.set_line_width(if high { 2.5 } else { 2.0 });
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        // Orbit rings behind the link.
        colour(cr, line, if high { 0.0 } else { 0.25 });
        for radius in [34.0, 54.0] {
            cr.arc(150.0, 65.0, radius, 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();
        }
        // The laptop: a screen and a base.
        colour(cr, "dl_surface", 1.0);
        rounded(cr, 38.0, 38.0, 70.0, 46.0, 6.0);
        let _ = cr.fill_preserve();
        colour(cr, line, 1.0);
        let _ = cr.stroke();
        rounded(cr, 28.0, 88.0, 90.0, 6.0, 3.0);
        let _ = cr.fill();
        // The phone.
        colour(cr, "dl_surface", 1.0);
        rounded(cr, 208.0, 28.0, 36.0, 68.0, 8.0);
        let _ = cr.fill_preserve();
        colour(cr, line, 1.0);
        let _ = cr.stroke();
        cr.arc(226.0, 88.0, 2.0, 0.0, std::f64::consts::TAU);
        let _ = cr.fill();
        // The link between them.
        cr.set_dash(&[2.0, 6.0], 0.0);
        cr.move_to(116.0, 62.0);
        cr.line_to(200.0, 62.0);
        let _ = cr.stroke();
        cr.set_dash(&[], 0.0);
        // The badge: a filled circle with an up arrow.
        colour(cr, main, 1.0);
        cr.arc(150.0, 62.0, 18.0, 0.0, std::f64::consts::TAU);
        let _ = cr.fill();
        colour(cr, "dl_on_accent", 1.0);
        cr.set_line_width(3.5);
        cr.move_to(150.0, 71.0);
        cr.line_to(150.0, 54.0);
        cr.move_to(142.0, 61.0);
        cr.line_to(150.0, 53.0);
        cr.line_to(158.0, 61.0);
        let _ = cr.stroke();
    });
    area
}
