//! Numbers that must read left to right inside right-to-left text ("80×24",
//! "12 ms"): in an RTL window the unisolated run is reordered by the bidi
//! algorithm ("24x80", "ms 12"). In an LTR window the text is returned
//! unchanged, so nothing that quotes or pins it changes.
//!
//! May depend on: gtk (the default text direction).

/// Left-to-right isolate and its terminator (U+2066 / U+2069).
const LRI: char = '\u{2066}';
const PDI: char = '\u{2069}';

/// `text` as one left-to-right run when `rtl`, else `text` itself.
pub(crate) fn ltr_run_for(text: &str, rtl: bool) -> String {
    if rtl {
        format!("{LRI}{text}{PDI}")
    } else {
        text.to_string()
    }
}

/// [`ltr_run_for`] with the current default text direction.
pub(crate) fn ltr_run(text: &str) -> String {
    // GTK's statics may only be asked on its thread, after it started: a
    // plain thread (a test, the tray's bus) gets LTR, as no window is shown.
    let rtl = gtk::is_initialized_main_thread()
        && gtk::Widget::default_direction() == gtk::TextDirection::Rtl;
    ltr_run_for(text, rtl)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Isolated in RTL, untouched in LTR.
    #[test]
    fn a_numeric_run_is_isolated_only_in_a_right_to_left_window() {
        assert_eq!(ltr_run_for("12 ms", false), "12 ms");
        assert_eq!(ltr_run_for("80×24", true), "\u{2066}80×24\u{2069}");
    }
}
