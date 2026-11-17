//! Install source, skew, schedule, the address parser, the store and the keys.

use super::*;
use crate::update::install_source::*;
use crate::update::schedule::*;
use crate::update::skew::{skew, Skew};
use crate::update::store::Store;
use crate::update::url::Url;
use std::path::PathBuf;

fn env(exe: &str, os: &str) -> Env {
    Env {
        exe: PathBuf::from(exe),
        os_release: os.to_string(),
        home: Some(PathBuf::from("/home/me")),
        ..Env::default()
    }
}

#[test]
fn the_install_source_is_found_in_the_documented_order() {
    let mut e = env("/usr/bin/desklink", "ID=debian\n");
    assert_eq!(detect(&e), Source::Package(Family::Debian));
    e.flatpak_info = true;
    assert_eq!(detect(&e), Source::Flatpak, "flatpak wins over everything");
    e.flatpak_info = false;
    e.appimage = Some(PathBuf::from("/tmp/x.AppImage"));
    assert_eq!(detect(&e), Source::AppImage);
    assert_eq!(
        detect(&env("/nix/store/abc-desklink/bin/desklink", "")),
        Source::Nix
    );
    assert_eq!(
        detect(&env("/home/me/.local/bin/desklink", "ID=arch")),
        Source::Manual
    );
    assert_eq!(
        detect(&env("/home/me/.cargo/bin/desklink", "")),
        Source::Manual
    );
    assert_eq!(detect(&env("/opt/desklink/desklink", "")), Source::Manual);
    assert_eq!(detect(&Env::default()), Source::Manual);
}

#[test]
fn the_family_comes_from_id_and_id_like() {
    for (os, want) in [
        ("ID=arch", Family::Arch),
        ("ID=manjaro\nID_LIKE=arch", Family::Arch),
        ("ID=cachyos\nID_LIKE=\"arch\"", Family::Arch),
        ("ID=endeavouros\nID_LIKE=arch", Family::Arch),
        ("NAME=x\nID=ubuntu\nID_LIKE=debian", Family::Debian),
        ("ID=linuxmint\nID_LIKE=\"ubuntu debian\"", Family::Debian),
        ("ID=pop\nID_LIKE=\"ubuntu debian\"", Family::Debian),
        ("ID=fedora", Family::Fedora),
        ("ID=rocky\nID_LIKE=\"rhel centos fedora\"", Family::Fedora),
        (
            "ID=opensuse-tumbleweed\nID_LIKE=\"opensuse suse\"",
            Family::Suse,
        ),
        ("ID=sles\nID_LIKE=\"suse\"", Family::Suse),
        ("ID=gentoo", Family::Other),
        ("", Family::Other),
    ] {
        assert_eq!(family(os), want, "{os}");
    }
}

#[test]
fn the_commands_are_exactly_the_documented_ones() {
    let command = |s: Source| match s.guidance() {
        Guidance::Command { command, .. } => command,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        command(Source::Flatpak),
        "flatpak update derx06.desklink.com"
    );
    assert_eq!(command(Source::Package(Family::Arch)), "yay -Syu desklink");
    assert_eq!(
        command(Source::Package(Family::Debian)),
        "sudo apt update && sudo apt install --only-upgrade desklink"
    );
    assert_eq!(
        command(Source::Package(Family::Fedora)),
        "sudo dnf upgrade desklink"
    );
    assert_eq!(
        command(Source::Package(Family::Suse)),
        "sudo zypper refresh && sudo zypper update desklink"
    );
    assert!(matches!(
        Source::Package(Family::Other).guidance(),
        Guidance::Text(_)
    ));
    assert!(matches!(
        Source::Manual.guidance(),
        Guidance::ReleasePage(_)
    ));
    assert!(
        matches!(Source::AppImage.guidance(), Guidance::ReleasePage(t) if t.contains("can't update itself"))
    );
    assert_eq!(
        Source::Package(Family::Debian).label(),
        "Debian or Ubuntu package"
    );
    assert_eq!(Source::Package(Family::Arch).label(), "Arch (AUR)");
    assert!(Source::Flatpak.may_lag() && !Source::Manual.may_lag());
}

#[test]
fn nothing_in_install_source_can_run_a_command() {
    let text = crate::source_scan::production(include_str!("../install_source.rs"));
    for word in [
        "Command::new",
        "std::process",
        "spawn(",
        "system(",
        "gio::",
        "glib::spawn",
    ] {
        assert!(!text.contains(word), "{word}");
    }
}

#[test]
fn skew_is_major_minor_only_and_quiet_when_unsure() {
    let ours = v("0.3.0");
    assert_eq!(
        skew(ours, Some("0.2.4")),
        Some(Skew::PhoneOlder(v("0.2.4")))
    );
    assert_eq!(
        skew(ours, Some("0.4.0")),
        Some(Skew::PhoneNewer(v("0.4.0")))
    );
    assert_eq!(
        skew(ours, Some("1.0.0")),
        Some(Skew::PhoneNewer(v("1.0.0")))
    );
    assert_eq!(
        skew(v("1.0.0"), Some("0.9.9")),
        Some(Skew::PhoneOlder(v("0.9.9")))
    );
    for quiet in [
        Some("0.3.0"),
        Some("0.3.9"),
        Some("0.3.0 "),
        None,
        Some(""),
        Some("garbage"),
        Some("0.3"),
        Some("0.2.x"),
    ] {
        assert_eq!(skew(ours, quiet), None, "{quiet:?}");
    }
}

#[test]
fn the_schedule_is_a_day_with_jitter_and_a_bounded_backoff() {
    for seed in [0, 1, 12345, u64::MAX] {
        let d = next_delay(0, seed);
        assert!((DAY - DAY / 10..=DAY + DAY / 10).contains(&d), "{d}");
    }
    let steps: Vec<u64> = (1..=7).map(|n| next_delay(n, 9)).collect();
    assert_eq!(
        steps,
        [3_600, 10_800, 21_600, 43_200, 86_400, 86_400, 86_400]
    );
    assert_eq!(next_delay(u32::MAX, 0), 86_400);
}

#[test]
fn the_address_parser_is_strict() {
    let ok = Url::parse("https://Desklink-Updates.web.app/latest.json").unwrap();
    assert_eq!(
        (ok.host.as_str(), ok.target.as_str(), ok.https),
        ("desklink-updates.web.app", "/latest.json", true)
    );
    assert_eq!(Url::parse("https://example.com").unwrap().target, "/");
    assert_eq!(
        Url::parse("https://example.com?x=1").unwrap().target,
        "/?x=1"
    );
    assert_eq!(
        Url::parse("http://127.0.0.1:8099/a")
            .unwrap()
            .effective_port(),
        8099
    );
    for bad in [
        "",
        "example.com",
        "ftp://example.com",
        "https://",
        "https://user@example.com/",
        "https://a b/",
        "https://ex\u{e9}.com/",
        "https://example.com/a b",
        "https://example.com/#x",
        "https://[::1]/",
        "https://example.com:/",
        "https://example.com:0/",
        "https://example.com:99999/",
        "https://-bad.com/",
        "https://a..b/",
        "https://example.com\\@evil/",
        "//example.com/",
        "https://exa_mple.com/",
        "https://example.com/\n",
    ] {
        assert_eq!(Url::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn the_store_round_trips_atomically_and_rejects_bad_ids() {
    let path = scratch("store");
    let mut s = Store::load(&path);
    assert_eq!(s.install_id.len(), 32);
    assert_eq!(s.install_id_bytes().len(), 16);
    s.etag = Some("\"x\"".into());
    s.last_checked = 5;
    s.save(&path).unwrap();
    assert!(!path.with_extension("json.tmp").exists());
    assert_eq!(Store::load(&path), s);
    // Another install gets another id.
    assert_ne!(Store::load(&scratch("store2")).install_id, s.install_id);
    // Unknown keys from a newer build are ignored, not fatal.
    std::fs::write(
        &path,
        br#"{"install_id":"00000000000000000000000000000001","future":[1]}"#,
    )
    .unwrap();
    assert_eq!(
        Store::load(&path).install_id,
        "00000000000000000000000000000001"
    );
}

#[test]
fn the_production_keys_are_a_slot_the_owner_fills_and_empty_trusts_nothing() {
    // Until the owner's keys are in, no manifest verifies in a release build.
    let trusted = crate::update::keys::trusted();
    assert!(trusted
        .iter()
        .all(|k| ["test1", "test2", "k1", "k2"].contains(&k.kid.as_str())));
    assert!(trusted.len() <= 4);
}

#[test]
fn the_test_key_only_exists_in_debug_builds() {
    let source = include_str!("../keys.rs");
    let at = source.find("const TEST").unwrap();
    assert!(source[..at]
        .trim_end()
        .ends_with("#[cfg(debug_assertions)]"));
    assert!(source.contains("#[cfg(debug_assertions)]\n    keys.extend"));
}

#[test]
fn an_empty_build_address_means_inert() {
    // No address: no fetcher, so the engine reports NotConfigured and never fetches.
    assert!(crate::update::fetch::HttpsFetcher::new("").is_none());
    assert_eq!(
        crate::update::service::BUILD_URL.is_empty(),
        option_env!("DESKLINK_UPDATE_URL").is_none()
    );
}

#[test]
fn the_two_settings_default_to_on_and_the_lines_default_channel() {
    let s = crate::device_links::config::AppSettings::default();
    assert!(s.update_check);
    assert_eq!(s.update_channel, "");
    // A config written before the keys existed still loads with the defaults.
    let old: crate::device_links::config::AppSettings =
        serde_json::from_str("{\"stay_awake\":true}").unwrap();
    assert!(old.update_check && old.stay_awake);
}

#[test]
fn the_top_bar_values_follow_the_documented_states() {
    use crate::update::engine::Status;
    use crate::update::model::{Channel, Component, Decision};
    use crate::update::service::{top_bar, Snapshot};
    let c = Component {
        version: v("0.4.0"),
        released: None,
        min_supported: v("0.0.0"),
        required_reason: None,
        rollout: 100,
        notes: vec![],
        notes_url: None,
        release_url: None,
        tarball: None,
    };
    let snap = |status: Status, decision: Decision| Snapshot {
        status,
        last_check: Status::Idle,
        decision: decision.clone(),
        manual_decision: decision,
        installed: v("0.3.0"),
        channel: Channel::Beta,
        last_checked: 0,
        cache_age: Some(0),
        skipped: None,
        generation: 0,
    };
    let row = |s: Status, d: Decision| top_bar(&snap(s, d));
    assert_eq!(
        row(Status::Idle, Decision::Available(c.clone())),
        (true, "0.4.0".into(), false)
    );
    assert_eq!(
        row(Status::Idle, Decision::Snoozed(c.clone(), 9)),
        (true, "0.4.0".into(), false)
    );
    assert_eq!(
        row(Status::Idle, Decision::Required(c.clone())),
        (true, "0.4.0".into(), true)
    );
    assert_eq!(
        row(Status::Idle, Decision::Skipped(c.clone())),
        (false, String::new(), false)
    );
    assert_eq!(
        row(Status::Idle, Decision::UpToDate),
        (false, String::new(), false)
    );
    assert_eq!(
        row(Status::Disabled, Decision::Available(c.clone())),
        (false, String::new(), false)
    );
    assert_eq!(
        row(Status::NotConfigured, Decision::Available(c.clone())),
        (false, String::new(), false)
    );
    assert_eq!(
        row(
            Status::Failed {
                reason: "offline",
                since: 1
            },
            Decision::Available(c)
        ),
        (true, "0.4.0".into(), false)
    );
}

/// The whole release key set is the project's own: none of the shared test
/// keys (their private halves are published in the fixtures) may be in it.
#[test]
fn no_test_key_is_in_a_release_configuration() {
    let release = crate::update::keys::release_keys();
    for test_key in keys() {
        assert!(
            release.iter().all(|k| k.public != test_key.public),
            "test key {} is in the release set",
            test_key.kid
        );
    }
    // And the compiled-in test keys are fenced to debug builds in the source.
    let source = include_str!("../keys.rs");
    for (kid, key) in [("test1", "1ESE_"), ("test2", "gdqQH")] {
        let at = source
            .find(key)
            .unwrap_or_else(|| panic!("{kid} not found"));
        let fenced = source[..at]
            .rfind("#[cfg(debug_assertions)]")
            .is_some_and(|f| !source[f..at].contains("fn "));
        assert!(fenced, "{kid} must sit under #[cfg(debug_assertions)]");
    }
}

#[test]
fn a_damaged_state_file_keeps_its_ratchet_and_install_id() {
    let path = scratch("salvage");
    std::fs::write(
        &path,
        br#"{"install_id":"0a0b0c0d0e0f00112233445566778899","last_issued_at":1795000123,"etag":{"broken":"#,
    )
    .unwrap();
    let s = Store::load(&path);
    assert_eq!(
        (s.last_issued_at, s.install_id.as_str()),
        (1_795_000_123, "0a0b0c0d0e0f00112233445566778899")
    );
    // A type error in a known field is salvaged the same way.
    std::fs::write(
        &path,
        br#"{"install_id":"zz","last_issued_at":77,"failures":"many"}"#,
    )
    .unwrap();
    let s = Store::load(&path);
    assert_eq!(s.last_issued_at, 77);
    assert_eq!(s.install_id.len(), 32);
    // And the save is whole: no temp file left behind.
    s.save(&path).unwrap();
    assert!(!path.with_extension("json.tmp").exists());
}
