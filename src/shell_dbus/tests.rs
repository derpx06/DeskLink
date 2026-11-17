//! Tests of the top-bar interface: the device model, actions and switches,
//! the property variants, the sender checks, the extension's files, digests
//! and state, and pins on the sources (one way in, one signal).

use super::*;

#[test]
fn a_running_menu_with_its_files_gone_reads_off() {
    assert_eq!(status_of(true, true, true), ExtensionStatus::Active);
    assert_eq!(status_of(true, true, false), ExtensionStatus::Off);
    assert_eq!(status_of(false, false, true), ExtensionStatus::NeedsLogin);
    assert_eq!(status_of(false, true, true), ExtensionStatus::Off);
    assert_eq!(status_of(false, false, false), ExtensionStatus::Off);
}

/// W13: the top bar gets a phone's shown items with `base` on this
/// computer's clock; nothing while it is not ready or its notifications
/// are hidden.
#[test]
fn the_top_bar_gets_the_ongoing_items_on_this_computers_clock() {
    let info = crate::device_links::device_info::DeviceInfo::local("p".into(), "Pixel".into());
    let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
    device.trusted = true;
    device.feature_transport_ready = true;
    let (phone_at, now) = (5_000_000_i64, 9_000_000_i64);
    let mut body = serde_json::Map::new();
    body.insert("at".into(), serde_json::json!(phone_at));
    body.insert(
        "items".into(),
        serde_json::json!([{"id": "t", "appLabel": "Clock", "title": "Pasta", "kind": "timer",
            "chronometerBase": phone_at + 60_000, "countdown": true}]),
    );
    device.activity = Some(crate::device_links::activity::snapshot_from(&body, now));
    let settings = AppSettings::default();
    let one = activities_variant(std::slice::from_ref(&device), &settings, now);
    assert_eq!(one.n_children(), 1);
    let item = one.child_value(0);
    let dict = glib::VariantDict::new(Some(&item));
    let get = |key: &str| dict.lookup_value(key, None).unwrap();
    assert_eq!(get("title").str(), Some("Pasta"));
    assert_eq!(get("base").get::<i64>(), Some(now + 60_000), "this clock");
    assert_eq!(get("countdown").get::<bool>(), Some(true));
    device.feature_transport_ready = false;
    assert_eq!(
        activities_variant(std::slice::from_ref(&device), &settings, now).n_children(),
        0
    );
    device.feature_transport_ready = true;
    let mut hidden = settings;
    hidden.hidden_notifications.insert("p".into());
    assert_eq!(activities_variant(&[device], &hidden, now).n_children(), 0);
}

use crate::device_links::device::BatteryStatus;
use crate::device_links::device_info::DeviceInfo;

/// Unlock with the phone on for `phone1` only.
fn on() -> AppSettings {
    let mut settings = AppSettings::default();
    settings.unlock_devices.insert("phone1".into());
    settings
}

fn phone() -> DeviceView {
    let info = DeviceInfo {
        id: "phone1".into(),
        name: "Pixel".into(),
        device_type: "phone".into(),
        protocol_version: 9,
        incoming_capabilities: Vec::new(),
        outgoing_capabilities: Vec::new(),
        instance_id: None,
        app_version: None,
    };
    DeviceView::from_info(&info, "10.0.0.2".into(), true)
}

#[test]
fn a_bucket_sync_is_published_with_its_line_detail_and_fraction() {
    let _shared = crate::bucket_progress::test_lock();
    use crate::device_links::webrtc::transfer_board::TransferRow;
    let mut device = phone();
    device.id = "shell-bucket".into();
    assert_eq!(shell_device(&device, &AppSettings::default()).bucket, None);
    crate::bucket_progress::sample_for_test(vec![TransferRow {
        id: "shell-bucket-1".into(),
        device_id: "shell-bucket".into(),
        name: "film.mkv".into(),
        incoming: true,
        done: 300_000_000,
        total: 1_200_000_000,
        preparing: false,
        paused: false,
    }]);
    let mapped = shell_device(&device, &AppSettings::default());
    let bucket = mapped.bucket.clone().expect("published while syncing");
    let expected = format!(
        "Syncing 1 file · {} of {}",
        crate::bucket_progress::size_words(300_000_000),
        crate::bucket_progress::size_words(1_200_000_000)
    );
    assert_eq!(bucket.line, expected);
    assert_eq!(bucket.detail, "Waiting…", "no speed after one sample");
    assert_eq!(bucket.permille, 250);
    let list = devices_variant(&[mapped]);
    let dict = glib::VariantDict::new(Some(&list.child_value(0)));
    let entry = glib::VariantDict::new(dict.lookup_value("bucket", None).as_ref());
    assert_eq!(
        entry.lookup::<String>("line").unwrap().as_deref(),
        Some(expected.as_str())
    );
    assert_eq!(entry.lookup::<f64>("fraction").unwrap(), Some(0.25));
    crate::bucket_progress::sample_for_test(Vec::new());
    assert_eq!(shell_device(&device, &AppSettings::default()).bucket, None);
}

#[test]
fn a_device_maps_to_the_documented_dict() {
    let mut device = phone();
    device.status = DeviceStatus::Paired;
    device.feature_transport_ready = true;
    device.connection_path = Some("lan".into());
    device.connection_nearby = true;
    device.desktop_session = DesktopSession::Controlling;
    device.terminal_sessions_attached = 2;
    device.battery_status = Some(BatteryStatus {
        current_charge: 81,
        is_charging: true,
        battery_quantity: None,
        threshold_event: None,
        charge_time_remaining: None,
    });
    let mapped = shell_device(&device, &AppSettings::default());
    assert!(mapped.paired && mapped.reachable && mapped.ready && mapped.charging);
    assert!(!mapped.quiet);
    // Silent for a few seconds: `quiet` is on the wire for the top bar.
    device.quiet = true;
    assert!(shell_device(&device, &AppSettings::default()).quiet);
    device.feature_transport_ready = false;
    assert!(
        !shell_device(&device, &AppSettings::default()).quiet,
        "only a ready phone is checking"
    );
    device.feature_transport_ready = true;
    device.quiet = false;
    assert_eq!(
        (mapped.battery, mapped.screen, mapped.terminals),
        (81, "control", 2)
    );
    assert_eq!(mapped.path, "lan");

    let list = devices_variant(&[mapped]);
    assert_eq!(list.type_().as_str(), "aa{sv}");
    let dict = glib::VariantDict::new(Some(&list.child_value(0)));
    assert_eq!(
        dict.lookup::<String>("id").unwrap().as_deref(),
        Some("phone1")
    );
    assert_eq!(
        dict.lookup::<String>("type").unwrap().as_deref(),
        Some("phone")
    );
    assert_eq!(dict.lookup::<i32>("battery").unwrap(), Some(81));
    assert_eq!(dict.lookup::<u32>("terminals").unwrap(), Some(2));
    // The phone takes nothing yet (no capabilities): only what is local.
    assert_eq!(
        dict.lookup::<Vec<String>>("actions").unwrap().unwrap(),
        [
            "open",
            "new-terminal",
            "open-files",
            "open-notes",
            "open-clipboard",
            "open-snippets",
            "open-notifications",
            "open-contacts",
            "open-terminals",
            "open-settings",
            "unpair",
            "end-sessions"
        ]
    );

    let idle = shell_device(&phone(), &AppSettings::default());
    assert!(!idle.reachable && !idle.ready);
    assert_eq!(
        (idle.battery, idle.screen, idle.path.as_str()),
        (-1, "", "")
    );
}

#[test]
fn a_ringing_call_is_published_with_answer_and_decline() {
    use crate::device_links::device::{CallState, CallStatus, PhoneCall};
    let mut device = phone();
    device.status = DeviceStatus::Paired;
    device.feature_transport_ready = true;
    device.incoming_capabilities =
        vec![crate::device_links::packet::PACKET_TYPE_TELEPHONY_REQUEST.into()];
    device.call_status = Some(CallStatus {
        calls: vec![PhoneCall {
            call_id: "c1".into(),
            state: CallState::Ringing,
            contact_name: None,
            number: None,
            started_at: None,
            incoming: true,
        }],
        muted: false,
        silenced: false,
    });
    let mapped = shell_device(&device, &AppSettings::default());
    for action in ["call-answer", "call-decline", "call-silence"] {
        assert!(mapped.actions.contains(&action), "{action}");
    }
    let devices = [mapped.clone()];
    assert!(check(&devices, "phone1", "call-answer").is_ok());
    assert!(
        check(&devices, "phone1", "call-end").is_err(),
        "nothing to end"
    );
    let dict = glib::VariantDict::new(Some(&devices_variant(&devices).child_value(0)));
    let call = glib::VariantDict::new(Some(&dict.lookup_value("call", None).unwrap()));
    assert_eq!(
        call.lookup::<String>("state").unwrap().as_deref(),
        Some("ringing")
    );
    assert_eq!(
        call.lookup::<String>("caller").unwrap().as_deref(),
        Some("Incoming call"),
        "a private caller"
    );
    assert_eq!(call.lookup::<bool>("incoming").unwrap(), Some(true));

    device.feature_transport_ready = false;
    let stale = shell_device(&device, &AppSettings::default());
    assert!(stale.call.is_none() && !stale.actions.iter().any(|a| a.starts_with("call-")));
}

#[test]
fn actions_follow_readiness_and_sessions_and_invoke_checks_them() {
    let mut device = phone();
    device.trusted = false;
    assert_eq!(actions(&device, &on()), ["open"]);
    device.terminal_sessions_attached = 1;
    assert_eq!(actions(&device, &on()), ["open", "end-sessions"]);

    let devices = [shell_device(&device, &on())];
    assert!(check(&devices, "phone1", "open").unwrap().is_some());
    assert!(check(&devices, "phone1", "end-sessions").is_ok());
    assert!(check(&devices, "phone1", "ring").is_err(), "not ready");
    assert!(check(&devices, "phone1", "explode").is_err());
    assert!(check(&devices, "nobody", "open").is_err());
    assert!(check(&devices, "", "pair-new").unwrap().is_none());
    assert!(check(&devices, "", "end-sessions").is_ok());
    assert!(check(&devices, "", "ring").is_err());
    assert!(check(&devices, "", "lock-hand-off").is_ok());
    assert!(
        check(&devices, "phone1", "unlock-via-phone").is_err(),
        "not connected"
    );
}

#[test]
fn the_unlock_button_needs_the_setting_and_a_phone_on_this_network() {
    let mut device = phone();
    device.status = DeviceStatus::Paired;
    device.feature_transport_ready = true;
    device.connection_path = Some("lan".into());
    device.connection_nearby = true;
    assert!(actions(&device, &on()).contains(&"unlock-via-phone"));
    assert!(
        !actions(&device, &AppSettings::default()).contains(&"unlock-via-phone"),
        "setting off"
    );
    let mut other = AppSettings::default();
    other.unlock_devices.insert("another-phone".into());
    assert!(
        !actions(&device, &other).contains(&"unlock-via-phone"),
        "on for another phone only"
    );
    for path in ["direct", "turn", "relay"] {
        device.connection_path = Some(path.into());
        assert!(
            !actions(&device, &on()).contains(&"unlock-via-phone"),
            "{path}"
        );
    }
}

#[test]
fn only_state_1_counts_as_an_active_extension() {
    let info = |state: f64| {
        let dict = glib::VariantDict::new(None);
        dict.insert("state", state);
        dict
    };
    assert!(is_active(&info(1.0)));
    assert!(!is_active(&info(2.0)));
    assert!(!is_active(&info(6.0)));
    assert!(!is_active(&glib::VariantDict::new(None)));
}

#[test]
fn a_copy_is_ours_when_stamped_or_a_shipped_build() {
    let known = ["aaa", "bbb"];
    assert!(ours("zzz", true, &known), "stamped by us");
    assert!(
        ours("bbb", false, &known),
        "copied by hand, a shipped build"
    );
    assert!(!ours("zzz", false, &known), "edited by someone");
    // Every build is listed, so a hand copy of this one is known later:
    // changing the extension means adding its digest to KNOWN_BUILDS.
    let embedded = files_digest(
        EXTENSION_FILES
            .iter()
            .map(|(name, content)| (*name, Some(*content))),
    );
    assert!(
        KNOWN_BUILDS.contains(&embedded.as_str()),
        "add {embedded} to KNOWN_BUILDS"
    );
    assert!(
        KNOWN_BUILDS.contains(&"cff05ba7d6f7e20074ae6fa7406177e58ac4bf8901d07faace5f131f07da8878"),
        "the 2026-10-20 build (extension.js 6911 bytes)"
    );
}

#[test]
fn the_extension_is_written_once_and_enabled_once() {
    let dir = std::env::temp_dir().join(format!("desklink-ext-{}", uuid::Uuid::new_v4()));
    assert!(install_extension_files(&dir).unwrap(), "missing: written");
    assert!(!install_extension_files(&dir).unwrap(), "same: left alone");
    // An older copy of ours (its stamp matches what is there): replaced.
    std::fs::write(dir.join("extension.js"), "old").unwrap();
    let older = files_digest(EXTENSION_FILES.iter().map(|(name, content)| {
        (
            *name,
            Some(if *name == "extension.js" {
                "old"
            } else {
                *content
            }),
        )
    }));
    std::fs::write(dir.join(INSTALL_STAMP), older).unwrap();
    assert!(
        install_extension_files(&dir).unwrap(),
        "outdated: rewritten"
    );
    // A copy someone edited: kept, and the caller is told.
    std::fs::write(dir.join("extension.js"), "// my tweaks").unwrap();
    assert!(install_extension_files(&dir).is_err());
    assert_eq!(
        std::fs::read_to_string(dir.join("extension.js")).unwrap(),
        "// my tweaks"
    );
    let metadata = std::fs::read_to_string(dir.join("metadata.json")).unwrap();
    assert!(metadata.contains(EXTENSION_UUID) && metadata.contains("unlock-dialog"));
    let _ = std::fs::remove_dir_all(&dir);

    // Copied by hand (no stamp) and identical: stamped, nothing written.
    let dir = std::env::temp_dir().join(format!("desklink-ext-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, content) in EXTENSION_FILES {
        std::fs::write(dir.join(name), content).unwrap();
    }
    assert!(!install_extension_files(&dir).unwrap());
    assert!(dir.join(INSTALL_STAMP).is_file(), "stamped");
    let _ = std::fs::remove_dir_all(&dir);

    let enabled = with_extension(vec!["other@x".into()]);
    assert_eq!(enabled, ["other@x", EXTENSION_UUID]);
    assert_eq!(with_extension(enabled.clone()), enabled);
    // Turning off removes only ours.
    assert_eq!(without_extension(enabled), vec!["other@x".to_string()]);
}

#[test]
fn a_disabled_extension_no_longer_handles_notifications() {
    let caps = vec!["notifications".to_string()];
    assert!(handles(&caps, "notifications", Some(true)));
    assert!(handles(&caps, "notifications", None), "state not known yet");
    assert!(!handles(&caps, "notifications", Some(false)), "turned off");
    assert!(!handles(&caps, "calls", Some(true)), "never said so");
}

/// Every non-test file of `shell_dbus/` (mod.rs first), test items left out;
/// fails when a file of the module is missing from the list.
fn production_sources() -> String {
    crate::source_scan::module_sources(
        "shell_dbus",
        &[
            ("mod.rs", include_str!("mod.rs")),
            ("clients.rs", include_str!("clients.rs")),
            ("extension.rs", include_str!("extension.rs")),
            ("extension_files.rs", include_str!("extension_files.rs")),
            ("methods.rs", include_str!("methods.rs")),
            ("model.rs", include_str!("model.rs")),
            ("variants.rs", include_str!("variants.rs")),
        ],
    )
}

/// Every top-bar verb passes the sender check first (no path around it).
#[test]
fn the_sender_is_checked_before_any_verb_runs() {
    let source = &production_sources();
    let gate = source
        .find("is_the_shell_caller(&connection, sender)")
        .unwrap();
    let run = source.find("match call(&app, method, &params)").unwrap();
    assert!(gate < run, "the gate comes before the verbs");
    assert_eq!(
        source.matches("match call(&app, method, &params)").count(),
        1,
        "and there is one way in"
    );
}

#[test]
fn gnome_is_found_in_a_desktop_list() {
    assert!(is_gnome("GNOME"));
    assert!(is_gnome("ubuntu:GNOME"));
    assert!(!is_gnome("KDE"));
    assert!(!is_gnome(""));
}

/// Only the Shell may say it shows DeskLink's alerts (so nobody else on the
/// bus can mute them), and only for capabilities DeskLink knows.
#[test]
fn only_the_shell_may_say_hello_and_only_for_known_capabilities() {
    assert!(is_the_shell(":1.42", Some(":1.42")));
    assert!(!is_the_shell(":1.99", Some(":1.42")), "another client");
    assert!(!is_the_shell(":1.42", None), "no shell on the bus");
    assert!(!is_the_shell("", Some("")));
    let said = vec![
        "notifications".to_string(),
        "calls".to_string(),
        "calls".to_string(),
        "mute-everything".to_string(),
    ];
    assert_eq!(vetted(said), ["notifications", "calls"]);
}

/// A stranger's question spends the window: the Shell's one Hello inside
/// it is "throttled" (and tried again after it), not "not the Shell".
#[test]
fn a_hello_inside_the_window_is_retried_not_refused() {
    let mut cache = OwnerCache::default();
    let start = std::time::Instant::now();
    assert_eq!(
        check_the_shell(&mut cache, start, ":1.99", || Some(":1.7".into())),
        Check::NotTheShell
    );
    // The Shell restarted a moment later with a new name.
    assert_eq!(
        check_the_shell(&mut cache, start, ":1.8", || Some(":1.8".into())),
        Check::Throttled
    );
    let after = start + OWNER_ASK_EVERY + std::time::Duration::from_millis(100);
    assert_eq!(
        check_the_shell(&mut cache, after, ":1.8", || Some(":1.8".into())),
        Check::Shell
    );
}

#[test]
fn a_flood_of_callers_costs_one_bus_question_per_window() {
    let mut cache = OwnerCache::default();
    let start = std::time::Instant::now();
    let asked = std::cell::Cell::new(0);
    let ask = |owner: &'static str| {
        let asked = &asked;
        move || {
            asked.set(asked.get() + 1);
            Some(owner.to_string())
        }
    };
    // The Shell is asked once, then answered from memory.
    assert!(is_the_shell_cached(&mut cache, start, ":1.7", ask(":1.7")));
    assert!(is_the_shell_cached(&mut cache, start, ":1.7", ask(":1.7")));
    assert_eq!(asked.get(), 1);
    // A stranger repeating itself asks nothing more inside the window.
    for _ in 0..1000 {
        assert!(!is_the_shell_cached(
            &mut cache,
            start,
            ":1.99",
            ask(":1.7")
        ));
    }
    assert_eq!(asked.get(), 1);
    // After it, one fresh question: a restarted Shell has a new name.
    let later = start + OWNER_ASK_EVERY;
    assert!(is_the_shell_cached(&mut cache, later, ":1.8", ask(":1.8")));
    assert_eq!(asked.get(), 2);
    assert!(
        !is_the_shell_cached(&mut cache, later, ":1.7", ask(":1.8")),
        "the old name is gone"
    );
}

/// A phone that takes everything the window offers.
fn capable() -> DeviceView {
    use crate::device_links::packet::*;
    let mut device = phone();
    device.status = DeviceStatus::Paired;
    device.trusted = true;
    device.feature_transport_ready = true;
    device.incoming_capabilities = [
        PACKET_TYPE_FINDMYPHONE_REQUEST,
        PACKET_TYPE_LOCK_REQUEST,
        PACKET_TYPE_SHARE_REQUEST,
        PACKET_TYPE_CLIPBOARD,
        PACKET_TYPE_PING,
        PACKET_TYPE_MOUSEPAD_REQUEST,
    ]
    .map(String::from)
    .to_vec();
    device
}

#[test]
fn the_remote_offers_each_action_only_when_the_phone_takes_it() {
    let device = capable();
    let all = actions(&device, &AppSettings::default());
    for action in [
        "ring",
        "lock",
        "send-files",
        "send-link",
        "send-clipboard",
        "ping",
        "show-screen",
        "new-terminal",
        "insert-photo",
        "insert-photo-file",
        "insert-scan",
        "insert-qr",
        "open-files",
        "open-notes",
        "open-clipboard",
        "open-snippets",
        "open-notifications",
        "open-contacts",
        "open-terminals",
        "open-settings",
        "unpair",
    ] {
        assert!(all.contains(&action), "{action}");
    }
    assert!(!all.contains(&"media-prev"), "no music");
    // A phone that takes no ping, no screen and no pictures is not offered them.
    let mut bare = capable();
    bare.incoming_capabilities = vec![crate::device_links::packet::PACKET_TYPE_PING.into()];
    let few = actions(&bare, &AppSettings::default());
    assert!(few.contains(&"ping"));
    for action in ["ring", "lock", "send-files", "show-screen", "insert-scan"] {
        assert!(!few.contains(&action), "{action}");
    }
    // Not paired: no pages, no unpair.
    let mut stranger = capable();
    stranger.trusted = false;
    let list = actions(&stranger, &AppSettings::default());
    assert!(!list.contains(&"unpair") && !list.contains(&"open-notes"));
    // Not ready: nothing that needs the phone.
    let mut offline = capable();
    offline.feature_transport_ready = false;
    let list = actions(&offline, &AppSettings::default());
    assert!(!list.contains(&"ring") && !list.contains(&"insert-qr"));
    // Every action is allowed through `check`, and only for its device.
    let devices = [shell_device(&device, &AppSettings::default())];
    assert!(check(&devices, "phone1", "ping").is_ok());
    assert!(check(&devices, "", "ping").is_err());
    assert!(check(&devices, "phone1", "send-link").is_ok());
}

#[test]
fn music_gets_a_previous_track_too() {
    let mut device = capable();
    device.media_status = Some(crate::device_links::device::MediaStatus {
        player: "Spotify".into(),
        player_list: vec!["Spotify".into()],
        title: "Song".into(),
        artist: "Band".into(),
        album: String::new(),
        is_playing: true,
        volume: None,
        position: None,
        length: None,
        can_seek: false,
        position_at: None,
        seek_hold: None,
        album_art_url: None,
        album_art: None,
    });
    let list = actions(&device, &AppSettings::default());
    for action in ["media-prev", "media-play-pause", "media-next"] {
        assert!(list.contains(&action), "{action}");
    }
    // No length or no seeking: the bar is not offered, and says why.
    assert!(!list.contains(&"media-seek"));
    assert!(!shell_media(&device).unwrap().seek_reason.is_empty());
    let media = device.media_status.as_mut().unwrap();
    media.length = Some(200_000);
    media.can_seek = true;
    media.position = Some(30_000);
    let list = actions(&device, &AppSettings::default());
    assert!(list.contains(&"media-seek"));
    let shown = shell_media(&device).unwrap();
    assert_eq!((shown.position_ms, shown.length_ms), (30_000, 200_000));
    assert_eq!(VERSION, 11);
}

/// The one table behind `SetSwitch`: every key resolves, a switch that is
/// off for security never turns on from the top bar, and the scope is kept.
#[test]
fn a_switch_resolves_to_its_command_and_security_ones_only_go_off() {
    for spec in &SWITCHES {
        let id = if spec.scope == Scope::Phone { "p" } else { "" };
        let off = resolve_switch(spec.key, id, false);
        assert!(off.is_ok(), "{} off", spec.key);
        let on = resolve_switch(spec.key, id, true);
        assert_eq!(on.is_err(), spec.sensitive, "{} on", spec.key);
        // The wrong scope never resolves.
        let wrong = if spec.scope == Scope::Phone { "" } else { "p" };
        assert!(
            resolve_switch(spec.key, wrong, false).is_err(),
            "{}",
            spec.key
        );
    }
    assert!(matches!(
        resolve_switch("showNotifications", "p", false),
        Ok(DaemonCommand::SetShowNotifications(id, false)) if id == "p"
    ));
    assert!(matches!(
        resolve_switch("terminalAccess", "p", false),
        Ok(DaemonCommand::SetTerminalAccess(id, false)) if id == "p"
    ));
    assert!(matches!(
        resolve_switch("phoneMic", "p", true),
        Ok(DaemonCommand::Sound(id, what, true)) if id == "p" && what == "phone-mic"
    ));
    assert!(matches!(
        resolve_switch("keepAwake", "", true),
        Ok(DaemonCommand::SetStayAwake(true))
    ));
    assert!(matches!(
        resolve_switch("glide", "p", false),
        Ok(DaemonCommand::SetGlide(id, None)) if id == "p"
    ));
    for key in [
        "terminalAccess",
        "unlockNearby",
        "galleryAccept",
        "phoneKeepAwake",
        "glide",
    ] {
        assert!(resolve_switch(key, "p", true).is_err(), "{key}");
    }
    assert!(resolve_switch("deskMic", "", true).is_err());
    assert!(resolve_switch("explode", "p", true).is_err());
    assert!(
        resolve_switch("walkAway", "", true).is_err(),
        "needs a phone"
    );
}

#[test]
fn a_switch_needs_a_paired_phone_and_sound_a_live_link() {
    let mut device = capable();
    let mapped = shell_device(&device, &AppSettings::default());
    assert!(switch_allowed(Some(&mapped), "deskSound", true).is_ok());
    assert!(switch_allowed(None, "keepAwake", true).is_ok());
    device.feature_transport_ready = false;
    let offline = shell_device(&device, &AppSettings::default());
    assert!(switch_allowed(Some(&offline), "deskSound", true).is_err());
    assert!(
        switch_allowed(Some(&offline), "deskSound", false).is_ok(),
        "off is always possible"
    );
    device.trusted = false;
    let stranger = shell_device(&device, &AppSettings::default());
    assert!(switch_allowed(Some(&stranger), "commandAlerts", false).is_err());
}

#[test]
fn the_settings_sound_and_counts_are_published() {
    let mut settings = AppSettings::default();
    settings.hidden_notifications.insert("phone1".into());
    settings.dnd_devices.insert("phone1".into());
    settings.unlock_devices.insert("phone1".into());
    let mut device = capable();
    device.terminal_access = true;
    device.rtt_ms = Some(12);
    device.sound = Some(crate::device_links::device::SoundUse {
        sending: Some("system".into()),
        hearing: Some("mic".into()),
        hearing_live: true,
    });
    let mapped = shell_device(&device, &settings);
    let list = devices_variant(&[mapped]);
    let dict = glib::VariantDict::new(Some(&list.child_value(0)));
    let switches = glib::VariantDict::new(dict.lookup_value("settings", None).as_ref());
    let flag = |key: &str| switches.lookup::<bool>(key).unwrap();
    assert_eq!(flag("showNotifications"), Some(false), "hidden here");
    assert_eq!(flag("dndShare"), Some(true));
    assert_eq!(flag("unlockNearby"), Some(true));
    assert_eq!(flag("terminalAccess"), Some(true));
    assert_eq!(flag("walkAway"), Some(false));
    assert_eq!(flag("glide"), Some(false));
    let sound = glib::VariantDict::new(dict.lookup_value("sound", None).as_ref());
    assert_eq!(sound.lookup::<bool>("deskSound").unwrap(), Some(true));
    assert_eq!(sound.lookup::<bool>("phoneMic").unwrap(), Some(true));
    assert_eq!(sound.lookup::<bool>("phoneSound").unwrap(), Some(false));
    assert_eq!(dict.lookup::<i32>("rttMs").unwrap(), Some(12));
    assert_eq!(dict.lookup::<u32>("unread").unwrap(), Some(0));
    assert_eq!(
        dict.lookup_value("transfers", None)
            .unwrap()
            .type_()
            .as_str(),
        "aa{sv}"
    );
    let desk = Desk {
        keep_awake: false,
        clipboard: true,
        hand_off: false,
        pill: false,
        forward_notifications: true,
        pause_media_on_call: false,
        desk_mic: false,
        sound_available: true,
        dnd_supported: false,
        has_password: true,
        update_available: false,
        update_version: String::new(),
        update_required: false,
    }
    .to_variant();
    let desk = glib::VariantDict::new(Some(&desk));
    assert_eq!(
        desk.lookup::<bool>("forwardNotifications").unwrap(),
        Some(true)
    );
    assert_eq!(desk.lookup::<bool>("hasPassword").unwrap(), Some(true));
}

#[test]
fn a_transfer_in_flight_is_published_with_its_lane() {
    use crate::device_links::webrtc::transfer_board;
    let row = transfer_board::TransferRow {
        id: "shell-t1".into(),
        device_id: "shell-lane".into(),
        name: "film.mkv".into(),
        incoming: false,
        done: 250,
        total: 1000,
        preparing: false,
        paused: false,
    };
    assert!(transfer_board::begin(row));
    transfer_board::set_lane("shell-t1", Some("fast lane"));
    let shown = shell_transfers("shell-lane");
    transfer_board::discard("shell-t1");
    assert_eq!(shown.len(), 1);
    assert_eq!(
        (shown[0].permille, shown[0].lane.as_str(), shown[0].incoming),
        (250, "fast lane", false)
    );
    assert!(shell_transfers("shell-lane").is_empty());
}

#[test]
fn every_page_action_names_a_page_the_window_has() {
    for (action, page) in PAGES {
        let (resolved, _) = crate::window::resolve_page(page);
        assert!(
            [
                "home",
                "files",
                "notes",
                "notifications",
                "contacts",
                "terminals",
                "settings"
            ]
            .contains(&resolved),
            "{action} -> {page}"
        );
    }
}

/// Phone names, notifications, snippets and permissions are for the Shell:
/// another process on the bus reads empty values of the same type, and
/// nothing is broadcast.
#[test]
fn only_the_shell_reads_the_private_properties() {
    let mut device = capable();
    device.name = "Secret phone".into();
    let settings = AppSettings::default();
    let devices = [shell_device(&device, &settings)];
    let values = std::collections::HashMap::from([
        ("Version", VERSION.to_variant()),
        ("Desk", desk(&devices).to_variant()),
        ("Devices", devices_variant(&devices)),
        ("Notifications", notifications_variant(&[device], &settings)),
        ("Transfers", transfers_variant(&[])),
        ("Snippets", snippets_variant()),
        ("Activities", activities_variant(&[], &settings, 0)),
    ]);
    for (name, real) in &values {
        let seen = visible_property(name, &values, true);
        assert_eq!(&seen, real, "{name} for the Shell");
        let other = visible_property(name, &values, false);
        assert_eq!(other.type_(), real.type_(), "{name}: the same type");
        if *name == "Version" {
            assert_eq!(other, *real);
        } else {
            assert_eq!(other.n_children(), 0, "{name} is empty for anyone else");
            assert!(!other.print(true).contains("Secret"), "{name}");
        }
    }
    assert!(
        visible_property("Devices", &values, true)
            .print(true)
            .contains("Secret phone"),
        "the Shell sees the name"
    );
    // No broadcast: every PropertiesChanged names its destination.
    let source = &production_sources();
    assert_eq!(source.matches(".emit_signal(").count(), 1);
    assert!(source.contains("emit_signal(\n        Some(destination),"));
}

/// The top bar gets a notification's words as one paragraph: its tray and
/// reply dialog would show every blank line of a long message.
#[test]
fn the_top_bar_gets_a_notifications_words_as_one_paragraph() {
    let info = crate::device_links::device_info::DeviceInfo::local("p".into(), "Pixel".into());
    let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
    device.trusted = true;
    device.notifications = vec![crate::device_links::device::DeviceNotification {
        id: "n".into(),
        app_name: "WhatsApp".into(),
        title: "Writers' Circle".into(),
        text: "Hours left!\n\nChoose:\n♦ Home\n♦ The Last Train\n\nHappy writing!".into(),
        ticker: String::new(),
        is_clearable: true,
        request_reply_id: None,
        actions: Vec::new(),
        silent: false,
        time: 0,
        app_icon: None,
    }];
    let list = notifications_variant(&[device], &AppSettings::default());
    let body = glib::VariantDict::new(Some(&list.child_value(0)))
        .lookup::<String>("body")
        .unwrap()
        .unwrap();
    assert_eq!(
        body,
        "Hours left! Choose: ♦ Home ♦ The Last Train Happy writing!"
    );
}

/// Notification buttons speak sentence case, like every other label
/// (UI-REDESIGN 5.1): "Log out now", "Turn on", "Call back", never "Log Out
/// Now". Every `add_button` of the files that post notifications is read; a
/// label that is not a literal (a loop over a list, a `format!` with a host
/// name) is counted and pinned per file, so a new one gets looked at.
#[test]
fn notification_buttons_are_sentence_case() {
    // (file, its text, buttons whose label is a variable: what they say)
    let files: [(&str, &str, usize); 5] = [
        ("shell_dbus/extension.rs", include_str!("extension.rs"), 0),
        (
            "application/indicator.rs",
            include_str!("../application/indicator.rs"),
            1,
        ), // "Allow", "Not now"
        (
            "application/mod.rs",
            include_str!("../application/mod.rs"),
            0,
        ),
        ("window/calls.rs", include_str!("../window/calls.rs"), 1), // "Answer", "Decline", "Silence"
        (
            "bucket_progress.rs",
            include_str!("../bucket_progress.rs"),
            0,
        ),
    ];
    let mut checked = 0;
    for (name, text, variables) in files {
        let source = crate::source_scan::production(text);
        let mut found_variables = 0;
        for call in source.split("add_button").skip(1) {
            let Some(args) = call
                .strip_prefix('(')
                .or_else(|| call.strip_prefix("_with_target_value("))
            else {
                continue;
            };
            let args = args.trim_start();
            let literal = args
                .strip_prefix("&format!(\"")
                .or_else(|| args.strip_prefix('"'))
                .and_then(|rest| rest.split('"').next());
            let Some(label) = literal else {
                found_variables += 1;
                continue;
            };
            checked += 1;
            let title_case = label
                .split_whitespace()
                .skip(1)
                // Names: GNOME's Files app and DeskLink's Bucket.
                .filter(|word| !["Files", "Bucket"].contains(word))
                .any(|word| word.chars().next().is_some_and(char::is_uppercase));
            assert!(!title_case, "{name}: \"{label}\" is Title Case");
        }
        assert_eq!(
            found_variables, variables,
            "{name}: buttons labelled from a variable"
        );
    }
    assert!(checked >= 12, "the scan found the buttons ({checked})");
}

/// Interface 11: the Desk dict carries the three read-only update values
/// under the names the extension reads, and `ShowUpdate` takes no argument.
#[test]
fn the_desk_dict_carries_the_update_values_and_show_update_has_no_arguments() {
    let dict = glib::VariantDict::new(Some(&desk(&[]).to_variant()));
    for key in ["update_available", "update_version", "update_required"] {
        assert!(dict.contains(key), "{key}");
    }
    assert!(XML.contains("<method name=\"ShowUpdate\"/>"));
}
