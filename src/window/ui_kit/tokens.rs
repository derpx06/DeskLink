//! The shared design tokens (`docs/UI-REDESIGN.md` 2.1), value for value the
//! phone's `DeskLinkColors`. Every name is `dl_*`: the older palette in
//! `application.rs` keeps serving the windows that are not redesigned yet,
//! and the redesign waves switch a window over by using these.
//!
//! Emitted by [`css`] as `@define-color dl_x` (libadwaita 1.5) and, from 1.6,
//! `--dl-x` on `:root`, next to the palette and swapped with it when the
//! style changes (light, dark, high contrast).

/// (name, light, dark). The phone's field is in the comment.
pub(crate) const TOKENS: &[(&str, &str, &str)] = &[
    // bg, sidebar, surface (cards), surface2 (tonal fill, key caps, icon squares).
    ("dl_bg", "#f7f7f4", "#131315"),
    ("dl_sidebar", "#efefeb", "#17171a"),
    ("dl_surface", "#ffffff", "#1b1b1e"),
    ("dl_surface_2", "#ecece8", "#252529"),
    // hairline: decorative only. outline: fields, unchecked switch (3:1).
    ("dl_hairline", "rgba(0,0,0,0.07)", "rgba(255,255,255,0.07)"),
    ("dl_outline", "#8a8a92", "#6a6a74"),
    // text, textDim, hint; textFaint is for glyphs only, never text.
    ("dl_text", "#18181b", "#f2f2f3"),
    ("dl_text_dim", "#585860", "#a8a8b2"),
    ("dl_hint", "#6a6a72", "#9494a0"),
    ("dl_text_faint", "#8f8f97", "#71717a"),
    // inverse / onInverse: the toast.
    ("dl_inverse", "#2f3036", "#e6e6ea"),
    ("dl_on_inverse", "#f1f1f3", "#18181b"),
    // accent = actions and selection.
    ("dl_accent", "#2f52d0", "#a9b8ff"),
    ("dl_on_accent", "#ffffff", "#10183f"),
    ("dl_accent_text", "#2a4cc4", "#a9b8ff"),
    ("dl_accent_container", "#e2e8fb", "#262d52"),
    ("dl_on_accent_container", "#1b2f7a", "#dfe5ff"),
    // success (mint) = connected, done.
    ("dl_success", "#0f9b72", "#3fd1a2"),
    ("dl_success_text", "#0b7a59", "#3fd1a2"),
    ("dl_success_container", "#e1f4ec", "#14302a"),
    ("dl_on_success_container", "#0b7a59", "#3fd1a2"),
    // warn (amber) = waiting on you.
    ("dl_warn", "#c98a00", "#ffc24d"),
    ("dl_warn_text", "#8a5a00", "#f5b84a"),
    ("dl_warn_container", "#fbf1dc", "#33290f"),
    ("dl_on_warn_container", "#8a5a00", "#f5b84a"),
    // error (red) = failed, stop, unpair. on_error: text on the solid fill.
    ("dl_error", "#b42626", "#ff6b6b"),
    ("dl_on_error", "#ffffff", "#18181b"),
    ("dl_error_text", "#b42626", "#ff8a8a"),
    ("dl_error_container", "#fbe9e8", "#3a1d1f"),
    ("dl_on_error_container", "#b42626", "#ff8a8a"),
    // live (coral) = the other device is using this one. Always with an End.
    ("dl_live", "#e5533d", "#ff7c66"),
    ("dl_live_text", "#b03a25", "#ff9a88"),
    ("dl_live_container", "#fbebe7", "#34201c"),
    ("dl_on_live_container", "#b03a25", "#ff9a88"),
    // The edge of cards, tiles and fields: the hairline, a solid line in
    // high contrast (see HIGH_CONTRAST below).
    ("dl_edge", "rgba(0,0,0,0.07)", "rgba(255,255,255,0.07)"),
];

/// High contrast (`UI-REDESIGN.md` 2.1): (name, light, dark) replacing the
/// value above. Text goes to black/white and the semantic text colours to
/// at least 8:1; edges become solid.
pub(crate) const HIGH_CONTRAST: &[(&str, &str, &str)] = &[
    ("dl_text", "#000000", "#ffffff"),
    ("dl_text_dim", "#3a3a40", "#d0d0d8"),
    ("dl_hint", "#3a3a40", "#d0d0d8"),
    ("dl_outline", "#000000", "#ffffff"),
    ("dl_hairline", "#3a3a40", "#d0d0d8"),
    ("dl_edge", "#000000", "#ffffff"),
    ("dl_accent_text", "#1a2f9e", "#c4ceff"),
    ("dl_on_accent_container", "#1a2f9e", "#c4ceff"),
    ("dl_success_text", "#005a3f", "#6fe8bd"),
    ("dl_on_success_container", "#005a3f", "#6fe8bd"),
    ("dl_warn_text", "#6b4300", "#ffd27a"),
    ("dl_on_warn_container", "#6b4300", "#ffd27a"),
    ("dl_error_text", "#8f1111", "#ffa3a3"),
    ("dl_on_error_container", "#8f1111", "#ffa3a3"),
    ("dl_live_text", "#8a2a18", "#ffb5a8"),
    ("dl_on_live_container", "#8a2a18", "#ffb5a8"),
];

/// Extra rules in high contrast only (GTK CSS has no contrast media query
/// on libadwaita 1.5): every card, tile, field and chip gets a solid 1.5 px
/// edge, and tonal fills become outlines.
pub(crate) const HIGH_CONTRAST_RULES: &str = "\
.dl-card, .dl-tile, .dl-chip-face, .dl-segmented, .dl-pill, .dl-btn-outline, .dl-btn-tonal, .dl-row-group {\
 box-shadow: 0 0 0 1.5px @dl_edge; }\n\
.dl-tile, .dl-chip-face, .dl-btn-tonal, .dl-pill { background-image: none; background-color: transparent; }\n\
.dl-tile.primary { background-color: @dl_accent; }\n";

/// The value of `name` in a style, high contrast included.
pub(crate) fn value(name: &str, dark: bool, contrast: bool) -> Option<&'static str> {
    let pick = |&(_, light, dark_value): &(&str, &'static str, &'static str)| {
        if dark {
            dark_value
        } else {
            light
        }
    };
    if contrast {
        if let Some(entry) = HIGH_CONTRAST.iter().find(|(n, _, _)| *n == name) {
            return Some(pick(entry));
        }
    }
    TOKENS.iter().find(|(n, _, _)| *n == name).map(pick)
}

/// The tokens as CSS for one style: `@define-color` always, `:root`
/// variables when libadwaita is 1.6+, and in high contrast the extra rules.
pub(crate) fn css(dark: bool, modern: bool, contrast: bool) -> String {
    let mut css = String::new();
    let mut root = String::new();
    for (name, _, _) in TOKENS {
        let value = value(name, dark, contrast).unwrap_or("transparent");
        css.push_str(&format!("@define-color {name} {value};\n"));
        root.push_str(&format!("--{}: {value}; ", name.replace('_', "-")));
    }
    if modern {
        css.push_str(&format!(":root {{ {root}}}\n"));
    }
    if contrast {
        css.push_str(HIGH_CONTRAST_RULES);
    }
    css
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "#rrggbb" or "rgba(r,g,b,a)" as 0..1 channels and alpha.
    fn parse(colour: &str) -> ([f64; 3], f64) {
        if let Some(hex) = colour.strip_prefix('#') {
            let channel =
                |at: usize| f64::from(u8::from_str_radix(&hex[at..at + 2], 16).unwrap()) / 255.0;
            return ([channel(0), channel(2), channel(4)], 1.0);
        }
        let inner = colour.trim_start_matches("rgba(").trim_end_matches(')');
        let parts: Vec<f64> = inner
            .split(',')
            .map(|part| part.trim().parse().unwrap())
            .collect();
        (
            [parts[0] / 255.0, parts[1] / 255.0, parts[2] / 255.0],
            parts[3],
        )
    }

    /// `name` as an opaque colour: a translucent one is painted over `under`.
    fn solid(name: &str, dark: bool, contrast: bool, under: [f64; 3]) -> [f64; 3] {
        let (rgb, alpha) = parse(value(name, dark, contrast).unwrap_or_else(|| panic!("{name}")));
        [0, 1, 2].map(|i| rgb[i] * alpha + under[i] * (1.0 - alpha))
    }

    fn luminance(rgb: [f64; 3]) -> f64 {
        let linear = |c: f64| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(rgb[0]) + 0.7152 * linear(rgb[1]) + 0.0722 * linear(rgb[2])
    }

    fn ratio(a: [f64; 3], b: [f64; 3]) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    /// Checks `fg` on `bg` in all three styles against the minimum for each
    /// (`high` is for high contrast, where the semantic colours reach 7).
    fn check(pairs: &[(&str, &str)], light_dark: f64, high: f64) {
        for (fg, bg) in pairs {
            for (dark, contrast) in [(false, false), (true, false), (false, true), (true, true)] {
                let ground = solid(bg, dark, contrast, [0.0; 3]);
                let r = ratio(solid(fg, dark, contrast, ground), ground);
                let wanted = if contrast { high } else { light_dark };
                assert!(
                    r >= wanted,
                    "{fg} on {bg} ({}{}) is {r:.2}:1, wanted {wanted}",
                    if dark { "dark" } else { "light" },
                    if contrast { ", high contrast" } else { "" }
                );
            }
        }
    }

    const GROUNDS: [&str; 4] = ["dl_bg", "dl_sidebar", "dl_surface", "dl_surface_2"];

    #[test]
    fn names_are_unique_dl_names_with_a_value_per_style() {
        let mut seen = std::collections::HashSet::new();
        for (name, light, dark) in TOKENS {
            assert!(name.starts_with("dl_"), "{name}");
            assert!(seen.insert(*name), "{name} twice");
            parse(light);
            parse(dark);
        }
        for (name, light, dark) in HIGH_CONTRAST {
            assert!(seen.contains(name), "{name} is not a token");
            parse(light);
            parse(dark);
        }
    }

    /// Text reads at 4.5:1 or more on every ground (7:1 in high contrast).
    #[test]
    fn text_tokens_meet_4_5_to_1_on_every_ground() {
        let mut pairs = Vec::new();
        for text in ["dl_text", "dl_text_dim", "dl_hint"] {
            for ground in GROUNDS {
                pairs.push((text, ground));
            }
        }
        for text in [
            "dl_accent_text",
            "dl_success_text",
            "dl_warn_text",
            "dl_error_text",
            "dl_live_text",
        ] {
            pairs.push((text, "dl_bg"));
            pairs.push((text, "dl_surface"));
        }
        check(&pairs, 4.5, 7.0);
    }

    /// Text on its own container, the filled buttons and the toast.
    #[test]
    fn container_and_fill_pairs_meet_4_5_to_1() {
        check(
            &[
                ("dl_on_accent_container", "dl_accent_container"),
                ("dl_accent_text", "dl_accent_container"),
                ("dl_on_success_container", "dl_success_container"),
                ("dl_on_warn_container", "dl_warn_container"),
                ("dl_on_error_container", "dl_error_container"),
                ("dl_on_live_container", "dl_live_container"),
                ("dl_on_accent", "dl_accent"),
                ("dl_on_error", "dl_error"),
                ("dl_on_inverse", "dl_inverse"),
            ],
            4.5,
            // High contrast keeps the fills; their text is at least AA.
            4.5,
        );
    }

    /// Graphics (the outline of a field, a status dot, the focus ring) read
    /// at 3:1 or more on the grounds they sit on (WCAG 1.4.11).
    #[test]
    fn graphics_meet_3_to_1() {
        let mut pairs = vec![
            ("dl_outline", "dl_bg"),
            ("dl_outline", "dl_surface"),
            ("dl_accent_text", "dl_bg"),
            ("dl_accent_text", "dl_surface"),
            // Glyphs only (never text): 3.2:1 on a card.
            ("dl_text_faint", "dl_surface"),
        ];
        for dot in ["dl_success", "dl_error", "dl_live"] {
            pairs.push((dot, "dl_surface"));
        }
        check(&pairs, 3.0, 3.0);
    }

    /// The amber fill is a dot beside a word, so it is held to the dot rule
    /// on the card only in dark, and by its word everywhere (warn_text is
    /// checked above); this pins what the light fill really measures.
    #[test]
    fn the_light_amber_dot_is_a_known_2_5_to_1_graphic() {
        let r = ratio(
            solid("dl_warn", false, false, [0.0; 3]),
            solid("dl_surface", false, false, [0.0; 3]),
        );
        assert!((2.0..3.0).contains(&r), "warn dot is {r:.2}:1");
        let dark = ratio(
            solid("dl_warn", true, false, [0.0; 3]),
            solid("dl_surface", true, false, [0.0; 3]),
        );
        assert!(dark >= 3.0, "{dark:.2}");
    }

    #[test]
    fn css_defines_every_token_and_high_contrast_swaps_values() {
        let light = css(false, true, false);
        let dark = css(true, true, false);
        let high = css(false, true, true);
        for (name, _, _) in TOKENS {
            assert!(light.contains(&format!("@define-color {name} ")), "{name}");
            assert!(light.contains(&format!("--{}:", name.replace('_', "-"))));
        }
        assert!(light.contains("@define-color dl_accent #2f52d0;"));
        assert!(dark.contains("@define-color dl_accent #a9b8ff;"));
        assert!(high.contains("@define-color dl_text #000000;"));
        assert!(high.contains(".dl-card"));
        assert!(
            !light.contains(".dl-card"),
            "the extra rules are high contrast only"
        );
        assert!(!css(false, false, false).contains(":root"));
    }
}
