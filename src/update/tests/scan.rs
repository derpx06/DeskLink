//! Source scans: what the update module may not know and who may call it.

use crate::source_scan::production;

const FILES: [(&str, &str); 13] = [
    ("engine.rs", include_str!("../engine.rs")),
    ("fetch.rs", include_str!("../fetch.rs")),
    ("install_source.rs", include_str!("../install_source.rs")),
    ("keys.rs", include_str!("../keys.rs")),
    ("mod.rs", include_str!("../mod.rs")),
    ("model.rs", include_str!("../model.rs")),
    ("open.rs", include_str!("../open.rs")),
    ("schedule.rs", include_str!("../schedule.rs")),
    ("service.rs", include_str!("../service.rs")),
    ("skew.rs", include_str!("../skew.rs")),
    ("store.rs", include_str!("../store.rs")),
    ("strict_json.rs", include_str!("../strict_json.rs")),
    ("url.rs", include_str!("../url.rs")),
];

fn module() -> String {
    crate::source_scan::module_sources("update", &FILES)
}

/// The request and the state carry nothing about the person or a phone.
#[test]
fn the_update_code_knows_no_pairing_identity_or_network_name() {
    assert!(!production(include_str!("../model.rs")).contains("Config::"));
    let text = module().to_ascii_lowercase();
    for word in [
        "device_id",
        "certificate",
        "fingerprint",
        "hostname",
        "gethostname",
        "instance_id",
        "peer",
        "pairing",
        "webrtc",
        "transport",
    ] {
        // The doc comments say what the module does not touch; skip comment lines.
        let code: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!code.contains(word), "update/ mentions {word}");
    }
}

#[test]
fn the_update_code_never_notifies_spawns_or_executes() {
    let text = module();
    for word in [
        "desk_notifications",
        "Notification",
        "std::process",
        "Command::new",
        "glib::spawn",
        "libc::",
    ] {
        assert!(!text.contains(word), "{word}");
    }
}

#[test]
fn nothing_reads_the_install_id_outside_the_store_engine_and_service() {
    for (name, text) in FILES {
        let code = production(text);
        if !["store.rs", "engine.rs", "tests"].contains(&name) && name != "model.rs" {
            assert!(!code.contains("install_id"), "{name}");
        }
    }
}

/// The only importers of `crate::update` are the application start-up, the
/// top-bar properties and (phase 2) `window/updates`.
#[test]
fn only_the_allowed_places_call_into_update() {
    fn walk(dir: &std::path::Path, hits: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, hits);
            } else if path.extension().is_some_and(|e| e == "rs")
                && !crate::source_scan::is_test_file(&path)
            {
                let rel = path
                    .strip_prefix(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                if rel.starts_with("update/") || rel == "main.rs" {
                    continue;
                }
                if production(&std::fs::read_to_string(&path).unwrap()).contains("crate::update") {
                    hits.push(rel);
                }
            }
        }
    }
    let mut hits = Vec::new();
    walk(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut hits,
    );
    hits.sort();
    // The exact files: the application start, the top bar's read-only
    // properties and (with the UI) the popup module.
    let allowed = |h: &str| {
        matches!(h, "application/mod.rs" | "shell_dbus/model.rs")
            || h.starts_with("window/updates/")
    };
    let stray: Vec<&String> = hits.iter().filter(|h| !allowed(h)).collect();
    assert!(
        stray.is_empty(),
        "update is a leaf; unexpected callers: {stray:?}"
    );
}

/// Release builds read the address from the build only.
#[test]
fn a_release_build_has_no_runtime_address_override() {
    let service = production(include_str!("../service.rs"));
    let at = service
        .find("std::env::var(\"DESKLINK_UPDATE_URL\")")
        .unwrap();
    assert!(service[..at]
        .trim_end()
        .lines()
        .rev()
        .take(3)
        .any(|l| l.contains("#[cfg(debug_assertions)]")));
    assert_eq!(service.matches("env::var").count(), 1);
    assert!(
        !production(include_str!("../fetch.rs")).contains("env::var"),
        "no proxy variable, no override"
    );
}
