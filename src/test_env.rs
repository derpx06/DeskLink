//! Test-only: a test process never touches the person's real files.
//!
//! Before `main` (an `.init_array` entry, so before any test thread, GLib or
//! `dirs` call can read the environment), the test binary points `HOME`,
//! `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME` and `XDG_RUNTIME_DIR`
//! at a fresh folder of its own under the temporary directory. Every writer
//! that finds its place through them (`dirs::config_dir`, `glib::user_*_dir`,
//! `$XDG_RUNTIME_DIR/desklink/terminal.sock`, the last device, the discovery
//! endpoints, the app-icons cache, the trust store) then writes there. A plain
//! `cargo test` once overwrote the real `~/.config/desklink/last-device`.
//!
//! `XDG_CACHE_HOME` goes to one shared `~/.cache/desklink-tests` instead, so
//! GStreamer's registry and fontconfig's cache are not rebuilt by every test
//! process; nothing DeskLink keeps there is the person's.
//!
//! May depend on: std only.

use std::path::PathBuf;

/// The variable that keeps the real home, for the guard test.
pub(crate) const REAL_HOME: &str = "DESKLINK_TEST_REAL_HOME";

#[used]
#[link_section = ".init_array"]
static ISOLATE: extern "C" fn() = isolate;

extern "C" fn isolate() {
    let real_home = std::env::var_os("HOME").map(PathBuf::from);
    let base = std::env::temp_dir().join(format!("desklink-test-home-{}", std::process::id()));
    for (var, dir) in [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "runtime"),
    ] {
        let path = base.join(dir);
        let _ = std::fs::create_dir_all(&path);
        set_private(&path);
        std::env::set_var(var, &path);
    }
    let cache = real_home
        .as_ref()
        .map(|home| home.join(".cache/desklink-tests"))
        .unwrap_or_else(|| base.join("cache"));
    let _ = std::fs::create_dir_all(&cache);
    std::env::set_var("XDG_CACHE_HOME", &cache);
    if let Some(home) = real_home {
        std::env::set_var(REAL_HOME, home);
    }
}

fn set_private(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    fn real_home() -> Option<PathBuf> {
        std::env::var_os(super::REAL_HOME).map(PathBuf::from)
    }

    fn not_real(what: &str, path: &Path) {
        let Some(home) = real_home() else { return };
        for real in [".config", ".local/share", ".local/state"] {
            assert!(
                !path.starts_with(home.join(real)),
                "{what} is the real {}: {}",
                real,
                path.display()
            );
        }
        assert_ne!(path, home, "{what} is the real home");
    }

    /// Every place a test could write the person's settings, trust store or
    /// state resolves inside the test's own folder.
    #[test]
    fn no_test_resolves_the_real_config_root() {
        let base = std::env::temp_dir();
        for (what, path) in [
            ("dirs::config_dir", dirs::config_dir()),
            ("dirs::data_dir", dirs::data_dir()),
            ("dirs::state_dir", dirs::state_dir()),
            ("dirs::home_dir", dirs::home_dir()),
            ("dirs::runtime_dir", dirs::runtime_dir()),
        ] {
            let path = path.unwrap_or_else(|| panic!("{what} is not set"));
            not_real(what, &path);
            assert!(
                path.starts_with(&base),
                "{what} outside the test folder: {}",
                path.display()
            );
        }
        not_real("glib::user_config_dir", &gtk::glib::user_config_dir());
        not_real("glib::user_data_dir", &gtk::glib::user_data_dir());
        not_real("glib::home_dir", &gtk::glib::home_dir());
        not_real(
            "desklink_config_root",
            &crate::device_links::config::desklink_config_root(),
        );
    }
}
