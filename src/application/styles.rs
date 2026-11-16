//! The brand styles: the palette and tokens (light, dark, high contrast), the
//! accent CSS, bundled fonts and the saved appearance.
//!
//! Sibling edges: none.

use super::*;

/// The palette, light and dark: (name, light, dark), from the DeskLink design
/// (`dl` / `dd`). libadwaita's own colours come first, then DeskLink's tokens
/// used by `style.css`. The neutrals (window, header bar, sidebar, cards,
/// popovers, dialogs) are the UI kit's `dl_bg`, `dl_sidebar` and `dl_surface`
/// (`UI-REDESIGN.md` 2.1), so a window not redesigned yet sits on the same
/// greys as one that is.
pub(super) const PALETTE: &[(&str, &str, &str)] = &[
    ("window_bg_color", "#f7f7f4", "#131315"),
    ("window_fg_color", "#18181b", "#f2f2f3"),
    ("view_bg_color", "#ffffff", "#1b1b1e"),
    ("view_fg_color", "#18181b", "#f2f2f3"),
    ("card_bg_color", "#ffffff", "#1b1b1e"),
    ("card_fg_color", "#18181b", "#f2f2f3"),
    ("headerbar_bg_color", "#f7f7f4", "#131315"),
    ("headerbar_fg_color", "#18181b", "#f2f2f3"),
    ("headerbar_backdrop_color", "#f7f7f4", "#131315"),
    ("sidebar_bg_color", "#efefeb", "#17171a"),
    ("sidebar_fg_color", "#18181b", "#f2f2f3"),
    ("sidebar_backdrop_color", "#efefeb", "#17171a"),
    // An inner split view's list (Notes): between the sidebar and the window,
    // in the same warm greys, not libadwaita's cool default.
    ("secondary_sidebar_bg_color", "#f3f3f0", "#151518"),
    ("secondary_sidebar_fg_color", "#18181b", "#f2f2f3"),
    ("secondary_sidebar_backdrop_color", "#f3f3f0", "#151518"),
    ("popover_bg_color", "#ffffff", "#1b1b1e"),
    ("popover_fg_color", "#18181b", "#f2f2f3"),
    ("dialog_bg_color", "#ffffff", "#1b1b1e"),
    ("dialog_fg_color", "#18181b", "#f2f2f3"),
    ("accent_bg_color", "#3b63e6", "#7c95ff"),
    ("accent_fg_color", "#ffffff", "#0d0e10"),
    ("accent_color", "#2f55d4", "#98abff"),
    ("success_bg_color", "#0f9b72", "#3fd1a2"),
    ("success_fg_color", "#ffffff", "#0d0e10"),
    ("success_color", "#0b7a5a", "#3fd1a2"),
    ("warning_bg_color", "#c98a00", "#ffc24d"),
    ("warning_fg_color", "#ffffff", "#0d0e10"),
    ("warning_color", "#9a6a00", "#ffc24d"),
    ("error_bg_color", "#c93030", "#ff6b6b"),
    ("error_fg_color", "#ffffff", "#0d0e10"),
    ("error_color", "#c93030", "#ff6b6b"),
    ("destructive_bg_color", "#c93030", "#ff6b6b"),
    ("destructive_fg_color", "#ffffff", "#0d0e10"),
    ("destructive_color", "#c93030", "#ff6b6b"),
];

/// DeskLink's own tokens: (name, light, dark). The design's names are in the
/// comments. `text_faint` is darker than the design's `t3` (#9a9aa2 /
/// #71717a), which read at under 3:1: every text colour here meets 4.5:1 on
/// the surfaces it is used on (see the contrast test).
pub(super) const TOKENS: &[(&str, &str, &str)] = &[
    // k: the soft fill of buttons, chips and icon squares.
    ("surface_2", "rgba(0,0,0,0.055)", "rgba(255,255,255,0.08)"),
    // l, b: hairlines and dashed edges.
    ("hairline", "rgba(0,0,0,0.07)", "rgba(255,255,255,0.07)"),
    (
        "hairline_strong",
        "rgba(0,0,0,0.14)",
        "rgba(255,255,255,0.16)",
    ),
    // t2, t3.
    ("text_dim", "#6b6b73", "#a1a1aa"),
    ("text_faint", "#6c6c74", "#8c8c95"),
    // m, o, ot, ob, obr: connected, and the other device in use.
    ("mint", "#0f9b72", "#3fd1a2"),
    ("coral", "#e5533d", "#ff7c66"),
    ("coral_text", "#b03a25", "#ff9a88"),
    ("coral_bg", "#fcefec", "#33221f"),
    (
        "coral_border",
        "rgba(229,83,61,0.25)",
        "rgba(255,124,102,0.3)",
    ),
    ("amber", "#9a6a00", "#ffc24d"),
    // av, avt: notification avatars.
    ("avatar_bg", "#e8ecf9", "#28305a"),
    ("avatar_fg", "#2f55d4", "#dfe5ff"),
    // rd, rdb: End Session, Unpair.
    ("danger", "#c93030", "#ff6b6b"),
    (
        "danger_bg",
        "rgba(211,58,58,0.09)",
        "rgba(255,107,107,0.12)",
    ),
    // sh: a card's edge and its soft shadow.
    ("card_edge", "rgba(0,0,0,0.06)", "rgba(255,255,255,0.06)"),
    ("card_shadow_near", "rgba(0,0,0,0.04)", "transparent"),
    // The terminal stays dark in both styles: it is the hero object.
    ("terminal_bg", "#0e0f11", "#0e0f11"),
    ("terminal_fg", "#d9dbe0", "#d9dbe0"),
    ("terminal_lines", "#8b9099", "#8b9099"),
    ("terminal_dim", "#7c818b", "#7c818b"),
    ("terminal_prompt", "#86d3a8", "#86d3a8"),
    ("terminal_coral", "#ff9a88", "#ff9a88"),
];

/// A token's colour in the current style, for widgets that draw themselves
/// (the link glyph, the battery ring). Unknown names are opaque grey.
pub(crate) fn token_color(name: &str) -> gtk::gdk::RGBA {
    let dark = adw::StyleManager::default().is_dark();
    PALETTE
        .iter()
        .chain(TOKENS)
        .find(|(token, _, _)| *token == name)
        .map(|(_, light, dark_value)| if dark { *dark_value } else { *light })
        .or_else(|| crate::window::ui_kit::tokens::value(name, dark, false))
        .and_then(|value| gtk::gdk::RGBA::parse(value).ok())
        .unwrap_or(gtk::gdk::RGBA::new(0.5, 0.5, 0.5, 1.0))
}

/// The CSS for one style. libadwaita 1.5 reads named colours; 1.6+ reads CSS
/// variables (`--window-bg-color`), so those are set too when present. In
/// high contrast libadwaita's own colours are kept and only our tokens are
/// set, with stronger edges.
pub(super) fn palette_css(dark: bool, modern: bool, contrast: bool) -> String {
    let pick =
        |light: &'static str, dark_value: &'static str| if dark { dark_value } else { light };
    let mut css = String::new();
    let mut root = String::new();
    if !contrast {
        for (name, light, dark_value) in PALETTE.iter().filter(|(name, _, _)| !is_accent(name)) {
            let value = pick(light, dark_value);
            css.push_str(&format!("@define-color {name} {value};\n"));
            root.push_str(&format!("--{}: {value}; ", name.replace('_', "-")));
        }
    }
    for (name, light, dark_value) in TOKENS {
        let value = match *name {
            "hairline" | "hairline_strong" | "card_edge" if contrast => "alpha(currentColor, 0.6)",
            "text_dim" | "text_faint" if contrast => "alpha(currentColor, 0.85)",
            "card_shadow_near" if contrast => "transparent",
            _ => pick(light, dark_value),
        };
        css.push_str(&format!("@define-color {name} {value};\n"));
    }
    if modern && !root.is_empty() {
        css.push_str(&format!(":root {{ {root}}}\n"));
    }
    // The UI kit's `dl_*` tokens (docs/UI-REDESIGN.md 2.1).
    css.push_str(&crate::window::ui_kit::tokens::css(dark, modern, contrast));
    css
}

/// libadwaita's accent colours, which DeskLink keeps as its own blue.
fn is_accent(name: &str) -> bool {
    name.starts_with("accent_")
}

/// The accent part of the palette. It is loaded above the user's own
/// `gtk.css` as well as the system accent: a user stylesheet that sets
/// `--accent-bg-color` (as accent-colour tweaks do) otherwise turned
/// DeskLink's buttons, switches and links that colour while the rest of
/// the palette stayed DeskLink's. High contrast keeps libadwaita's.
pub(super) fn accent_css(dark: bool, modern: bool, contrast: bool) -> String {
    if contrast {
        return String::new();
    }
    let mut css = String::new();
    let mut root = String::new();
    for (name, light, dark_value) in PALETTE.iter().filter(|(name, _, _)| is_accent(name)) {
        let value = if dark { dark_value } else { light };
        css.push_str(&format!("@define-color {name} {value};\n"));
        root.push_str(&format!("--{}: {value}; ", name.replace('_', "-")));
    }
    if modern {
        css.push_str(&format!(":root {{ {root}}}\n"));
    }
    css
}

/// Whether the running GTK parses CSS custom properties (`:root { --x: }`,
/// GTK 4.16 with libadwaita 1.6). Older ones (Ubuntu 24.04: GTK 4.14,
/// libadwaita 1.5) get the `@define-color` names alone.
pub(crate) fn modern_css() -> bool {
    adw::minor_version() >= 6
}

/// Loads the palette for the current style and keeps it in step: light,
/// dark or high contrast, as the system or the Appearance setting changes.
pub(super) fn install_brand_styles() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let modern = modern_css();
    let palette = gtk::CssProvider::new();
    // Above `style.css`, whose token fallbacks this replaces.
    gtk::style_context_add_provider_for_display(
        &display,
        &palette,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    // The UI kit's classes, after `style.css`; their colours are the
    // palette's `dl_*` names.
    let kit = gtk::CssProvider::new();
    kit.load_from_resource("/derx06/desklink/com/ui-kit.css");
    gtk::style_context_add_provider_for_display(
        &display,
        &kit,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    // The Files area's look, after the kit's.
    let files = gtk::CssProvider::new();
    files.load_from_resource("/derx06/desklink/com/files-ui.css");
    gtk::style_context_add_provider_for_display(
        &display,
        &files,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let accent = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(
        &display,
        &accent,
        gtk::STYLE_PROVIDER_PRIORITY_USER + 1,
    );
    let update = move |manager: &adw::StyleManager| {
        let (dark, contrast) = (manager.is_dark(), manager.is_high_contrast());
        palette.load_from_string(&palette_css(dark, modern, contrast));
        accent.load_from_string(&accent_css(dark, modern, contrast));
    };
    let manager = adw::StyleManager::default();
    manager.set_color_scheme(appearance_scheme(&load_appearance()));
    update(&manager);
    let update = std::rc::Rc::new(update);
    manager.connect_dark_notify({
        let update = update.clone();
        move |manager| update(manager)
    });
    manager.connect_high_contrast_notify(move |manager| update(manager));
    // Xfce, MATE and Cinnamon go dark through the GTK theme alone.
    crate::theme_hint::follow(|| {
        if load_appearance() == "system" {
            adw::StyleManager::default().set_color_scheme(appearance_scheme("system"));
        }
    });
}

/// Makes the bundled JetBrains Mono (SIL OFL 1.1, licence beside it in the
/// resources) available to this process, for `.mono` text. Fontconfig needs
/// a file, so the font is copied once into DeskLink's cache directory.
pub(super) fn register_bundled_fonts() {
    #[link(name = "fontconfig")]
    extern "C" {
        fn FcConfigAppFontAddFile(
            config: *mut std::ffi::c_void,
            file: *const std::os::raw::c_uchar,
        ) -> std::os::raw::c_int;
    }
    let Ok(bytes) = gio::resources_lookup_data(
        "/derx06/desklink/com/fonts/JetBrainsMono.ttf",
        gio::ResourceLookupFlags::NONE,
    ) else {
        return;
    };
    let Some(dir) = dirs::cache_dir().map(|dir| dir.join("desklink")) else {
        return;
    };
    let path = dir.join("JetBrainsMono.ttf");
    let current = std::fs::metadata(&path).is_ok_and(|meta| meta.len() == bytes.len() as u64);
    if !current
        && (std::fs::create_dir_all(&dir).is_err() || std::fs::write(&path, &*bytes).is_err())
    {
        eprintln!("[UI] Could not cache the bundled monospace font");
        return;
    }
    let Ok(file) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return;
    };
    // SAFETY: a null config means fontconfig's current one; the path is a
    // valid NUL-terminated string that outlives the call, which copies it.
    let added = unsafe { FcConfigAppFontAddFile(std::ptr::null_mut(), file.as_ptr().cast()) };
    if added == 0 {
        eprintln!("[UI] Fontconfig did not accept the bundled monospace font");
    }
}

/// Where the Appearance choice is kept: `~/.config/desklink/appearance`.
fn appearance_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("desklink").join("appearance"))
}

/// "system", "light" or "dark".
pub(crate) fn load_appearance() -> String {
    appearance_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|text| text.trim().to_string())
        .filter(|choice| matches!(choice.as_str(), "light" | "dark"))
        .unwrap_or_else(|| "system".to_string())
}

pub(crate) fn save_appearance(choice: &str) {
    if let Some(path) = appearance_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(error) = std::fs::write(&path, choice) {
            eprintln!("[UI] Could not save the appearance: {error}");
        }
    }
}

pub(crate) fn appearance_scheme(choice: &str) -> adw::ColorScheme {
    match choice {
        "light" => adw::ColorScheme::ForceLight,
        "dark" => adw::ColorScheme::ForceDark,
        _ => crate::theme_hint::system_scheme(),
    }
}
