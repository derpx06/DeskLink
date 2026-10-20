pub static VERSION: &str = env!("CARGO_PKG_VERSION");
pub static GETTEXT_PACKAGE: &str = "desklink";
// Meson passes its prefix's locale directory; a plain `cargo build` gets the system one.
pub static LOCALEDIR: &str = match option_env!("DESKLINK_LOCALEDIR") {
    Some(dir) => dir,
    None => "/usr/share/locale",
};
