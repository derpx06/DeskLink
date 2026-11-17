//! Tests of the trust store and settings: load and save, versions and
//! quarantine, trust and untrust (with candidates for a reinstalled phone),
//! the mirrors, removed-feature cleanup and the transfer-state move.

use super::*;

/// Settings written before clipboard sync and notification mirroring had
/// switches load with both on, and the defaults stay out of the file.
#[test]
fn clipboard_and_notifications_default_on_for_older_settings() {
    let old: AppSettings = serde_json::from_str(r#"{"stay_awake":true}"#).unwrap();
    assert!(old.clipboard_sync);
    assert!(old.shows_notifications("phone"));
    let written = serde_json::to_string(&AppSettings::default()).unwrap();
    assert!(!written.contains("hidden_notifications"), "{written}");

    assert!(old.alerts_commands_to("phone"), "command alerts default on");
    assert!(
        !old.unlocks_from("phone"),
        "unlocking from a phone defaults off"
    );
    assert!(!written.contains("unlock_devices") && !written.contains("muted_command_alerts"));
    let mut one = AppSettings::default();
    one.unlock_devices.insert("phone".into());
    one.muted_command_alerts.insert("tablet".into());
    assert!(
        one.unlocks_from("phone") && !one.unlocks_from("tablet"),
        "per phone"
    );
    assert!(one.alerts_commands_to("phone") && !one.alerts_commands_to("tablet"));
    assert!(old.recent_folders.is_empty());
    assert!(!written.contains("recent_folders"));

    let mut hidden = AppSettings::default();
    hidden.hidden_notifications.insert("phone".into());
    assert!(!hidden.shows_notifications("phone"));
    assert!(hidden.shows_notifications("tablet"));
}

/// With no relay built in and none entered, nothing tries to reach one.
#[test]
fn no_relay_is_wanted_until_one_is_set_up() {
    assert_eq!(relay_url_or("", ""), "");
    assert_eq!(relay_url_or("  ", "wss://built.in/v1"), "wss://built.in/v1");
    assert_eq!(
        relay_url_or(" wss://mine.example/v1 ", ""),
        "wss://mine.example/v1"
    );
    let mut settings = AppSettings::default();
    if DEFAULT_RELAY_URL.is_empty() {
        assert!(!settings.relay_configured());
        assert!(!settings.relay_wanted(), "empty default: client not wanted");
    }
    settings.relay_url = "wss://mine.example/v1".into();
    assert_eq!(
        settings.relay_wanted(),
        crate::device_links::relay_link::RELAY_ENABLED,
        "an address is enough only while the relay is compiled in"
    );
    settings.relay_enabled = false;
    assert_eq!(
        settings.relay_wanted(),
        crate::device_links::relay_link::RELAY_ENABLED,
        "the stored switch is not read: the constant is the only control"
    );
}

/// 0.3 ships LAN-only: with the switch off, even a stored relay address,
/// "on" and "Use TURN" (kept untouched in config.json) reach nothing.
#[test]
fn the_relay_compiled_out_ignores_every_stored_relay_setting() {
    use crate::device_links::relay_link::{client, RELAY_ENABLED};
    if RELAY_ENABLED {
        return;
    }
    let settings = AppSettings {
        relay_enabled: true,
        relay_url: "wss://mine.example/v1".into(),
        use_turn: true,
        ..AppSettings::default()
    };
    assert!(!settings.relay_configured());
    assert!(!settings.relay_wanted());
    assert!(!client::force_relay());
    let packet = crate::device_links::desk_features::settings_packet(&settings, "phone");
    assert_eq!(packet.get_bool("relayConfigured"), Some(false));
    assert_eq!(packet.get_bool("relay"), Some(false));
}

#[test]
fn local_identity_is_stable_after_reload() {
    let dir = std::env::temp_dir().join(format!("desklink-test-{}", Uuid::new_v4()));
    let first = Config::load_from_dir(dir.clone()).unwrap();
    let first_id = first.local_device_info().id;
    drop(first);

    let second = Config::load_from_dir(dir.clone()).unwrap();
    assert_eq!(second.local_device_info().id, first_id);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_certificate_that_no_longer_matches_the_key_is_rebuilt() {
    let dir = std::env::temp_dir().join(format!("desklink-mismatch-{}", Uuid::new_v4()));
    let first = Config::load_from_dir(dir.clone()).unwrap();
    let first_id = first.local_device_info().id;
    let stale_certificate = first.certificate().to_pem().unwrap();
    drop(first);

    // Losing the key while the certificate survives is the realistic case:
    // a partial restore, or a key wiped by a permissions repair.
    fs::remove_file(dir.join("privateKey.pem")).unwrap();

    let repaired = Config::load_from_dir(dir.clone()).unwrap();
    assert_eq!(repaired.local_device_info().id, first_id);
    assert_ne!(repaired.certificate().to_pem().unwrap(), stale_certificate);
    assert!(certificate_matches_key(
        repaired.certificate(),
        repaired.key()
    ));

    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn the_private_key_is_not_world_readable() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("desklink-perm-{}", Uuid::new_v4()));
    let config = Config::load_from_dir(dir.clone()).unwrap();
    drop(config);

    let mode = fs::metadata(dir.join("privateKey.pem"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "group and other must have no access");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn malformed_certificate_is_rebuilt_without_changing_identity() {
    let dir = std::env::temp_dir().join(format!("desklink-cert-test-{}", Uuid::new_v4()));
    let first = Config::load_from_dir(dir.clone()).unwrap();
    let first_id = first.local_device_info().id;
    drop(first);

    fs::write(dir.join("certificate.pem"), b"").unwrap();

    let repaired = Config::load_from_dir(dir.clone()).unwrap();
    assert_eq!(repaired.local_device_info().id, first_id);
    assert!(repaired
        .certificate()
        .to_pem()
        .unwrap()
        .starts_with(b"-----BEGIN CERTIFICATE-----"));

    let _ = fs::remove_dir_all(dir);
}

fn trusted_config(dir: &std::path::Path, id: &str, certificate: &str) -> Config {
    let mut config = Config::load_from_dir(dir.to_path_buf()).unwrap();
    config
        .trust_device(
            &DeviceInfo::local(id.to_string(), "Test phone".to_string()),
            certificate.to_string(),
        )
        .unwrap();
    config
}

/// Terminal access is a shell on this computer. It must be something the
/// user turned on, never something a pairing or an old config implies.
#[test]
fn terminal_access_is_off_by_default_and_for_configs_that_predate_it() {
    let dir = std::env::temp_dir().join(format!("desklink-term-default-{}", Uuid::new_v4()));
    let id = format!("phone-{}", Uuid::new_v4().simple());
    let config = trusted_config(&dir, &id, "cert-a");
    assert!(!terminal_access_allowed(&id));
    drop(config);

    // A config.json written before the field existed has no such key.
    let path = dir.join("config.json");
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    json["trusted_devices"][&id]
        .as_object_mut()
        .unwrap()
        .remove("terminal_access");
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();

    let reloaded = Config::load_from_dir(dir.clone()).unwrap();
    assert!(!reloaded.stored.trusted_devices[&id].terminal_access);
    assert!(!terminal_access_allowed(&id));
    let _ = fs::remove_dir_all(dir);
}

/// 0.3: what removed features stored is deleted on the first start, quietly;
/// pairing records, the certificate and every other preference stay.
#[test]
fn the_stored_data_of_removed_features_goes_on_first_start_and_nothing_else() {
    let dir = std::env::temp_dir().join(format!("desklink-removed-{}", Uuid::new_v4()));
    let id = format!("phone-{}", Uuid::new_v4().simple());
    let config = trusted_config(&dir, &id, "cert-a");
    let device_id = config.stored.device_id.clone();
    drop(config);
    // As a 1.2 install left things: sealed automation rules and their
    // key, and run-command and call-audio settings in config.json.
    let automations = dir.join("automations");
    fs::create_dir_all(&automations).unwrap();
    fs::write(automations.join("rules.sealed"), b"sealed").unwrap();
    fs::write(automations.join("key"), b"k").unwrap();
    let path = dir.join("config.json");
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    json["settings"]["run_commands"] =
        serde_json::json!([{"id": "a", "name": "Deploy", "command": "make"}]);
    json["settings"]["call_audio"] = true.into();
    json["settings"]["call_audio_devices"] = serde_json::json!({"p": "AA:BB"});
    json["settings"]["stay_awake"] = true.into();
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    let before = fs::read(dir.join("certificate.pem")).unwrap();

    let reloaded = Config::load_from_dir(dir.clone()).unwrap();
    assert!(!automations.exists(), "the rules and their key are gone");
    let written = fs::read_to_string(&path).unwrap();
    for gone in ["run_commands", "call_audio"] {
        assert!(!written.contains(gone), "{gone} still in config.json");
    }
    assert_eq!(reloaded.stored.device_id, device_id, "same device id");
    assert!(
        reloaded.stored.trusted_devices.contains_key(&id),
        "pairing kept"
    );
    assert!(
        reloaded.stored.settings.stay_awake,
        "other preferences kept"
    );
    assert_eq!(fs::read(dir.join("certificate.pem")).unwrap(), before);
    // A second start has nothing to remove and changes nothing.
    assert!(!remove_stored_removed_features(&dir));
    let _ = fs::remove_dir_all(dir);
}

/// A phone that pairs under a new id with the same name and type is only
/// a *candidate* for replacing the older record: nothing is deleted by
/// the trust store itself (review finding C).
#[test]
fn pairing_a_lookalike_queues_a_candidate_and_deletes_nothing() {
    let dir = std::env::temp_dir().join(format!("desklink-replace-{}", Uuid::new_v4()));
    let mut config = Config::load_from_dir(dir.clone()).unwrap();
    let info = |id: &str, name: &str, kind: &str| DeviceInfo {
        id: id.to_string(),
        name: name.to_string(),
        device_type: kind.to_string(),
        protocol_version: 9,
        incoming_capabilities: Vec::new(),
        outgoing_capabilities: Vec::new(),
        instance_id: None,
        app_version: None,
    };
    let tag = Uuid::new_v4().simple().to_string();
    let (old, fresh) = (format!("old-{tag}"), format!("new-{tag}"));
    let (other, tablet) = (format!("other-{tag}"), format!("tablet-{tag}"));
    let name = format!("Pixel {tag}");
    config
        .trust_device(&info(&old, &name, "phone"), "cert-old".into())
        .unwrap();
    config
        .trust_device(&info(&other, "Another phone", "phone"), "cert-o".into())
        .unwrap();
    config
        .trust_device(&info(&tablet, &name, "tablet"), "cert-t".into())
        .unwrap();
    let _ = take_replacement_candidates();
    // Re-confirming the same id (even with a new certificate) is nothing.
    config
        .trust_device(&info(&old, &name, "phone"), "cert-old-2".into())
        .unwrap();
    // (The queue is the process's: another test may have queued its own.)
    let ours = |candidates: Vec<ReplacementCandidate>| {
        candidates
            .into_iter()
            .filter(|candidate| candidate.name == name)
            .count()
    };
    assert_eq!(ours(take_replacement_candidates()), 0);
    // The lookalike: new id, same name and type.
    config
        .trust_device(&info(&fresh, &name, "phone"), "cert-new".into())
        .unwrap();
    assert!(config.is_trusted(&old), "not deleted here");
    assert!(config.is_trusted(&fresh));
    let mine: Vec<_> = take_replacement_candidates()
        .into_iter()
        .filter(|candidate| candidate.new_id == fresh)
        .collect();
    assert_eq!(
        mine,
        [ReplacementCandidate {
            old_id: old.clone(),
            new_id: fresh.clone(),
            name: name.clone(),
        }],
        "only the same name and type"
    );
    assert_eq!(ours(take_replacement_candidates()), 0, "taken once");
    let _ = fs::remove_dir_all(dir);
}

/// Settings -> Calls: the switch writes `pause_media_on_call` (what
/// `DaemonCommand::SetPauseMediaOnCall` does) and it survives a restart.
#[test]
fn the_calls_switch_for_pausing_music_is_saved() {
    let dir = std::env::temp_dir().join(format!("desklink-pause-{}", Uuid::new_v4()));
    let mut config = Config::load_from_dir(dir.clone()).unwrap();
    assert!(config.stored.settings.pause_media_on_call, "on by default");
    config
        .update_settings(|settings| settings.pause_media_on_call = false)
        .unwrap();
    drop(config);
    let reloaded = Config::load_from_dir(dir.clone()).unwrap();
    assert!(!reloaded.stored.settings.pause_media_on_call);
    let _ = fs::remove_dir_all(dir);
}

/// "Pause music during calls" is on unless turned off, also for a
/// config written before the switch existed.
#[test]
fn pausing_music_during_calls_defaults_on() {
    assert!(AppSettings::default().pause_media_on_call);
    let old: AppSettings = serde_json::from_str(r#"{"stay_awake": true}"#).unwrap();
    assert!(old.pause_media_on_call);
    let off: AppSettings = serde_json::from_str(r#"{"pause_media_on_call": false}"#).unwrap();
    assert!(!off.pause_media_on_call);
}

/// Settings default off, survive a restart, and load from a config that
/// predates them. (The process-wide mirror is shared by every test that
/// loads a config, so this checks what was persisted, not the mirror.)
#[test]
fn settings_persist_and_default_off_for_configs_that_predate_them() {
    let dir = std::env::temp_dir().join(format!("desklink-settings-{}", Uuid::new_v4()));
    let mut config = Config::load_from_dir(dir.clone()).unwrap();
    assert!(!config.stored.settings.stay_awake);
    config
        .update_settings(|settings| settings.stay_awake = true)
        .unwrap();
    drop(config);
    assert!(
        Config::load_from_dir(dir.clone())
            .unwrap()
            .stored
            .settings
            .stay_awake
    );

    let path = dir.join("config.json");
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    json.as_object_mut().unwrap().remove("settings");
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    assert!(
        !Config::load_from_dir(dir.clone())
            .unwrap()
            .stored
            .settings
            .stay_awake
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn terminal_access_persists_across_a_restart_and_can_be_revoked() {
    let dir = std::env::temp_dir().join(format!("desklink-term-persist-{}", Uuid::new_v4()));
    let id = format!("phone-{}", Uuid::new_v4().simple());
    let mut config = trusted_config(&dir, &id, "cert-a");

    config.set_terminal_access(&id, true).unwrap();
    assert!(terminal_access_allowed(&id));
    drop(config);

    // Simulate a fresh process: the mirror forgets, the file remembers.
    mirror_terminal_access(&id, false);
    let mut reloaded = Config::load_from_dir(dir.clone()).unwrap();
    assert!(terminal_access_allowed(&id), "consent was not persisted");

    reloaded.set_terminal_access(&id, false).unwrap();
    assert!(!terminal_access_allowed(&id));
    drop(reloaded);
    let again = Config::load_from_dir(dir.clone()).unwrap();
    assert!(!again.stored.trusted_devices[&id].terminal_access);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn terminal_access_cannot_be_granted_to_an_unpaired_device() {
    let dir = std::env::temp_dir().join(format!("desklink-term-unpaired-{}", Uuid::new_v4()));
    let mut config = Config::load_from_dir(dir.clone()).unwrap();
    assert!(config.set_terminal_access("never-paired", true).is_err());
    assert!(!terminal_access_allowed("never-paired"));
    let _ = fs::remove_dir_all(dir);
}

/// A remembered desktop grant follows the pairing exactly like terminal
/// consent: kept on re-confirmation, gone on unpair or a new certificate.
#[test]
fn a_remembered_desktop_grant_belongs_to_the_pairing() {
    let dir = std::env::temp_dir().join(format!("desklink-rd-grant-{}", Uuid::new_v4()));
    let id = format!("phone-{}", Uuid::new_v4().simple());
    let info = DeviceInfo::local(id.clone(), "Test phone".to_string());
    let mut config = trusted_config(&dir, &id, "cert-a");

    set_remote_desktop_token(&id, Some("t1"));
    set_remote_desktop_token(&id, Some("t2"));
    assert_eq!(
        remote_desktop_token(&id).as_deref(),
        Some("t2"),
        "a fresh token replaces the used one"
    );

    config.trust_device(&info, "cert-a".to_string()).unwrap();
    assert_eq!(
        remote_desktop_token(&id).as_deref(),
        Some("t2"),
        "re-confirming dropped the grant"
    );

    config.trust_device(&info, "cert-b".to_string()).unwrap();
    assert_eq!(
        remote_desktop_token(&id),
        None,
        "a new certificate inherited desktop control"
    );

    set_remote_desktop_token(&id, Some("t3"));
    config.untrust_device(&id).unwrap();
    assert_eq!(
        remote_desktop_token(&id),
        None,
        "unpairing kept desktop control"
    );
    let _ = fs::remove_dir_all(dir);
}

/// Re-confirming a pairing keeps the user's choice; unpairing, or a new
/// certificate behind the same id, resets it to off.
#[test]
fn consent_survives_reconfirmation_but_not_unpairing_or_a_new_certificate() {
    let dir = std::env::temp_dir().join(format!("desklink-term-trust-{}", Uuid::new_v4()));
    let id = format!("phone-{}", Uuid::new_v4().simple());
    let info = DeviceInfo::local(id.clone(), "Test phone".to_string());
    let mut config = trusted_config(&dir, &id, "cert-a");
    config.set_terminal_access(&id, true).unwrap();

    config.trust_device(&info, "cert-a".to_string()).unwrap();
    assert!(
        terminal_access_allowed(&id),
        "re-confirming revoked consent"
    );

    config.trust_device(&info, "cert-b".to_string()).unwrap();
    assert!(
        !terminal_access_allowed(&id),
        "a new certificate inherited consent"
    );

    config.set_terminal_access(&id, true).unwrap();
    config.untrust_device(&id).unwrap();
    assert!(!terminal_access_allowed(&id));
    config.trust_device(&info, "cert-b".to_string()).unwrap();
    assert!(!terminal_access_allowed(&id), "a re-pair inherited consent");
    let _ = fs::remove_dir_all(dir);
}

/// Everything a fresh config creates is for this user only: no group or
/// other bits on any file or directory (the session-password hash, every
/// pairing certificate and the partial transfers live here).
#[test]
fn a_fresh_config_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir()
        .join(format!(
            "desklink-private-config-{}",
            uuid::Uuid::new_v4().simple()
        ))
        .join("protocol");
    let config = Config::load_from_dir(dir.clone()).unwrap();
    config.save().unwrap();
    fn walk(path: &std::path::Path, bad: &mut Vec<String>) {
        let mode = fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bad.push(format!("{} {mode:o}", path.display()));
        }
        if path.is_dir() {
            for entry in fs::read_dir(path).unwrap().flatten() {
                walk(&entry.path(), bad);
            }
        }
    }
    let mut bad = Vec::new();
    walk(&dir, &mut bad);
    assert!(bad.is_empty(), "{bad:?}");
    let _ = fs::remove_dir_all(dir.parent().unwrap());
}

/// A remote unpair (the phone said `pair:false`) forgets the revs the desk
/// remembered, so the phone's snapshots are sent again after a re-pair:
/// with a stale `have` the contacts stayed empty.
#[test]
fn untrusting_a_device_forgets_the_snapshot_revs_it_sent() {
    use crate::device_links::packet::NetworkPacket;
    let dir = std::env::temp_dir().join(format!("desklink-forget-revs-{}", Uuid::new_v4()));
    let mut config = Config::load_from_dir(dir.clone()).unwrap();
    let id = format!("phone-{}", Uuid::new_v4().simple());
    let info = DeviceInfo {
        id: id.clone(),
        name: "Pixel".into(),
        device_type: "phone".into(),
        protocol_version: 9,
        incoming_capabilities: Vec::new(),
        outgoing_capabilities: Vec::new(),
        instance_id: None,
        app_version: None,
    };
    config.trust_device(&info, "cert-1".into()).unwrap();
    let mut snapshot = NetworkPacket::new("desklink.contacts");
    snapshot.set("rev", "R1");
    crate::device_links::sync::note_received(&id, Some("instance-a"), "contacts", &snapshot);
    let have = |id: &str| {
        crate::device_links::sync::have_packet(id, Some("instance-a"))
            .body
            .get("have")
            .and_then(|have| have.as_object())
            .is_some_and(|have| have.contains_key("contacts"))
    };
    assert!(have(&id), "remembered");
    config.untrust_device(&id).unwrap();
    assert!(!have(&id), "forgotten with the pairing");
    // A new identity behind the same id forgets too.
    config.trust_device(&info, "cert-1".into()).unwrap();
    crate::device_links::sync::note_received(&id, Some("instance-a"), "contacts", &snapshot);
    config.trust_device(&info, "cert-2".into()).unwrap();
    assert!(!have(&id));
    let _ = fs::remove_dir_all(dir);
}

/// A settings file damaged by something else is set aside (never deleted),
/// DeskLink starts fresh and says so in words, and the file the next start
/// reads is a good one.
#[test]
fn a_corrupt_settings_file_is_set_aside_and_explained() {
    let dir = std::env::temp_dir().join(format!("desklink-corrupt-{}", Uuid::new_v4()));
    let first = Config::load_from_dir(dir.clone()).unwrap();
    let id = first.local_device_id().to_string();
    drop(first);
    fs::write(dir.join("config.json"), b"{\"device_id\": \"abc\", \"trunc").unwrap();
    let mut config = Config::load_from_dir(dir.clone()).expect("a damaged file does not stop it");
    assert_ne!(config.local_device_id(), id, "a fresh identity");
    let notice = config.take_load_notice().expect("the person is told");
    assert!(
        notice.contains("damaged") && notice.contains("config.corrupt-"),
        "{notice}"
    );
    assert!(
        !notice.contains("EOF") && !notice.contains("serde"),
        "plain words: {notice}"
    );
    assert!(
        notice.contains("unpair") && notice.contains("pair again"),
        "tells the person to remove the old entry on the phone: {notice}"
    );
    let kept: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("config.corrupt-")
        })
        .collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(
        fs::read(kept[0].path()).unwrap(),
        b"{\"device_id\": \"abc\", \"trunc"
    );
    assert!(serde_json::from_slice::<serde_json::Value>(
        &fs::read(dir.join("config.json")).unwrap()
    )
    .is_ok());
    let _ = fs::remove_dir_all(dir);
}

/// The file has a version; fields this build does not know survive a save;
/// a file from a newer build is read but never rewritten.
#[test]
fn the_settings_file_is_versioned_forward_compatible_and_never_downgraded() {
    let dir = std::env::temp_dir().join(format!("desklink-version-{}", Uuid::new_v4()));
    let config = Config::load_from_dir(dir.clone()).unwrap();
    drop(config);
    let path = dir.join("config.json");
    let read = || serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(read()["version"], CONFIG_VERSION);
    // A field from a newer build, at the top and among the settings.
    let mut json = read();
    json["from_the_future"] = serde_json::json!({"a": 1});
    json["settings"]["glide_v2"] = serde_json::json!("keep me");
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    let mut config = Config::load_from_dir(dir.clone()).unwrap();
    config.set_device_name_for_test("Renamed");
    config.save().unwrap();
    let after = read();
    assert_eq!(after["from_the_future"]["a"], 1, "kept");
    assert_eq!(after["settings"]["glide_v2"], "keep me", "kept");
    assert_eq!(after["device_name"], "Renamed");
    // A newer file we can read is kept compatibly: its version and every
    // field we do not know survive our save.
    let mut newer = read();
    newer["version"] = (CONFIG_VERSION + 1).into();
    newer["device_name"] = "Newer".into();
    newer["added_in_v2"] = serde_json::json!([1, 2]);
    fs::write(&path, serde_json::to_vec(&newer).unwrap()).unwrap();
    let mut config = Config::load_from_dir(dir.clone()).unwrap();
    assert!(config
        .take_load_notice()
        .is_some_and(|n| n.contains("newer DeskLink")));
    config.set_device_name_for_test("Changed here");
    config.save().unwrap();
    let after = read();
    assert_eq!(after["version"], CONFIG_VERSION + 1, "not downgraded");
    assert_eq!(after["added_in_v2"], serde_json::json!([1, 2]));
    assert_eq!(after["device_name"], "Changed here", "and saved");
    let _ = fs::remove_dir_all(dir);
}

/// A newer file this build cannot even parse is "newer", not "damaged":
/// nothing is set aside or regenerated, the device id is kept, and saving
/// says it cannot.
/// Unknown fields inside a pairing record survive load and save.
#[test]
fn unknown_fields_in_a_pairing_record_survive_a_round_trip() {
    let dir = std::env::temp_dir().join(format!("desklink-extra-{}", Uuid::new_v4()));
    let mut config = Config::load_from_dir(dir.clone()).unwrap();
    let id = "abcdefgh_1234_abcdefgh_1234_abcd";
    config
        .trust_device(&DeviceInfo::local(id.into(), "Phone".into()), "PEM".into())
        .unwrap();
    drop(config);
    let path = dir.join("config.json");
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    json["trusted_devices"][id]["from_the_future"] = serde_json::json!({"nested": [1, 2, 3]});
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    let mut config = Config::load_from_dir(dir.clone()).unwrap();
    config.set_device_name_for_test("Renamed so that it saves");
    config.save().unwrap();
    let after: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        after["trusted_devices"][id]["from_the_future"],
        serde_json::json!({"nested": [1, 2, 3]})
    );
    assert_eq!(after["trusted_devices"][id]["name"], "Phone");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_unreadable_newer_file_is_not_treated_as_damage() {
    let dir = std::env::temp_dir().join(format!("desklink-newer-{}", Uuid::new_v4()));
    drop(Config::load_from_dir(dir.clone()).unwrap());
    let path = dir.join("config.json");
    let key_before = fs::read(dir.join("privateKey.pem")).unwrap();
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let id = json["device_id"].as_str().unwrap().to_string();
    json["version"] = (CONFIG_VERSION + 1).into();
    json["trusted_devices"] = serde_json::json!("a shape this build does not know");
    let bytes = serde_json::to_vec(&json).unwrap();
    fs::write(&path, &bytes).unwrap();
    let mut config = Config::load_from_dir(dir.clone()).unwrap();
    assert!(config
        .take_load_notice()
        .is_some_and(|n| n.contains("newer DeskLink")));
    assert_eq!(config.local_device_info().id, id, "same identity");
    assert!(config.save().unwrap_err().contains("newer DeskLink"));
    assert_eq!(fs::read(&path).unwrap(), bytes, "left exactly as it was");
    assert_eq!(fs::read(dir.join("privateKey.pem")).unwrap(), key_before);
    let aside = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().contains("corrupt"));
    assert!(!aside, "not set aside as damaged");
    let _ = fs::remove_dir_all(dir);
}

/// A damaged config.json starts a new identity with its own key and
/// certificate: the certificate names the new id (what the phone checks:
/// the CN must equal the id the peer claims), and the old pair is kept.
#[test]
fn a_new_identity_gets_a_matching_key_and_certificate_and_keeps_the_old_pair() {
    let dir = std::env::temp_dir().join(format!("desklink-newid-{}", Uuid::new_v4()));
    let first = Config::load_from_dir(dir.clone()).unwrap();
    let old_id = first.local_device_info().id;
    drop(first);
    fs::write(dir.join("config.json"), b"{ not json").unwrap();
    let second = Config::load_from_dir(dir.clone()).unwrap();
    let new_id = second.local_device_info().id;
    assert_ne!(new_id, old_id);
    let common_name = |config: &Config| {
        config
            .certificate()
            .subject_name()
            .entries_by_nid(openssl::nid::Nid::COMMONNAME)
            .next()
            .unwrap()
            .data()
            .to_string()
            .unwrap()
    };
    assert_eq!(common_name(&second), new_id, "CN == the id it will claim");
    assert!(certificate_matches_key(second.certificate(), second.key()));
    // And the same after a restart.
    drop(second);
    let third = Config::load_from_dir(dir.clone()).unwrap();
    assert_eq!(third.local_device_info().id, new_id);
    assert_eq!(common_name(&third), new_id);
    let kept = |prefix: &str| {
        fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
    };
    assert!(kept("privateKey.pem.old-") && kept("certificate.pem.old-"));
    let _ = fs::remove_dir_all(dir);
}

/// A copy that fails leaves the original alone; a truncated file from an
/// earlier failure is replaced by a verified one before the original goes.
#[test]
fn a_failed_or_truncated_copy_never_costs_the_only_good_partial() {
    let base = std::env::temp_dir().join(format!("desklink-verify-{}", Uuid::new_v4()));
    let (old, new) = (base.join("old"), base.join("new"));
    fs::create_dir_all(&old).unwrap();
    fs::create_dir_all(&new).unwrap();
    let good = vec![7u8; 100_000];
    fs::write(old.join("a.part"), &good).unwrap();
    // The target is in the way as a folder: the copy cannot be placed.
    fs::create_dir_all(new.join("a.part")).unwrap();
    migrate_transfer_state(&old, &new);
    assert_eq!(fs::read(old.join("a.part")).unwrap(), good, "original kept");
    assert!(
        fs::read_dir(&new)
            .unwrap()
            .flatten()
            .all(|entry| entry.file_name() != "a.migrating"),
        "no temp left"
    );
    // A truncated target from an earlier failed copy is not believed.
    fs::remove_dir_all(new.join("a.part")).unwrap();
    fs::write(new.join("a.part"), &good[..1000]).unwrap();
    migrate_transfer_state(&old, &new);
    assert_eq!(
        fs::read(new.join("a.part")).unwrap(),
        good,
        "replaced, verified"
    );
    assert!(!old.exists(), "and only then is the original removed");
    let _ = fs::remove_dir_all(&base);
}

/// S3: a move that stopped half way is finished at the next start.
#[test]
fn a_half_finished_transfer_state_move_is_finished_later() {
    let base = std::env::temp_dir().join(format!("desklink-migrate-{}", Uuid::new_v4()));
    let (old, new) = (base.join("old"), base.join("new"));
    fs::create_dir_all(&old).unwrap();
    fs::create_dir_all(&new).unwrap();
    fs::write(old.join("a.part"), "old a").unwrap();
    fs::write(old.join("b.part"), "old b").unwrap();
    fs::write(new.join("a.part"), "already moved").unwrap();
    migrate_transfer_state(&old, &new);
    assert!(!old.exists(), "the old folder goes once all is in the new");
    assert_eq!(
        fs::read_to_string(new.join("a.part")).unwrap(),
        "already moved"
    );
    assert_eq!(fs::read_to_string(new.join("b.part")).unwrap(), "old b");
    let _ = fs::remove_dir_all(&base);
}

/// A settings file that cannot be rewritten (a full or read-only disk)
/// does not stop DeskLink from starting.
#[test]
fn a_failed_save_at_start_is_not_fatal() {
    use std::os::unix::fs::PermissionsExt;
    if !crate::test_privileges::unprivileged(
        "device_links::config::tests::a_failed_save_at_start_is_not_fatal",
    ) {
        return;
    }
    let dir = std::env::temp_dir().join(format!("desklink-nosave-{}", Uuid::new_v4()));
    drop(Config::load_from_dir(dir.clone()).unwrap());
    // Different bytes than a save would write, so a save is wanted.
    let path = dir.join("config.json");
    let json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    let loaded = Config::load_from_dir(dir.clone());
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(loaded.is_ok(), "{:?}", loaded.err());
    let _ = fs::remove_dir_all(dir);
}

/// 0.3 cleans up after itself: the old restore-token file and the old
/// transfer folder go, quietly, once.
#[test]
fn leftover_tokens_and_old_transfer_state_are_removed() {
    let dir = std::env::temp_dir().join(format!("desklink-left-{}", Uuid::new_v4()));
    fs::create_dir_all(dir.join("transfers")).unwrap();
    fs::write(dir.join("transfers/a.json"), b"{}").unwrap();
    fs::write(dir.join("remote_desktop_tokens.json"), b"{}").unwrap();
    assert!(remove_stored_removed_features(&dir));
    assert!(!dir.join("transfers").exists());
    assert!(!dir.join("remote_desktop_tokens.json").exists());
    assert!(!remove_stored_removed_features(&dir), "once");
    let _ = fs::remove_dir_all(dir);
}

/// The partials move to the state directory in one go and only when the
/// new place is not there yet.
#[test]
fn transfer_partials_move_out_of_the_config_directory_once() {
    let base = std::env::temp_dir().join(format!("desklink-move-{}", Uuid::new_v4()));
    let (old, new) = (
        base.join("config/webrtc-transfers"),
        base.join("state/desklink/transfers"),
    );
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join(".desklink-1.part"), b"partial").unwrap();
    migrate_transfer_state(&old, &new);
    assert!(!old.exists());
    assert_eq!(fs::read(new.join(".desklink-1.part")).unwrap(), b"partial");
    // The new place exists: an old folder that appears again (a move that
    // stopped half way, a restored backup) is merged in and goes.
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("later"), b"x").unwrap();
    migrate_transfer_state(&old, &new);
    assert!(!old.exists());
    assert!(new.join("later").exists());
    let _ = fs::remove_dir_all(base);
}

/// The walk-away lock is off until turned on for a phone, stays out of
/// the file while off, and is gone with the pairing and with the identity.
#[test]
fn the_walk_away_lock_is_per_phone_off_by_default_and_does_not_survive_unpair() {
    let old: AppSettings = serde_json::from_str(r#"{"stay_awake":true}"#).unwrap();
    assert!(!old.locks_when_away("phone"));
    let written = serde_json::to_string(&AppSettings::default()).unwrap();
    assert!(!written.contains("walk_away_devices"), "{written}");

    let dir = std::env::temp_dir().join(format!("desklink-walk-{}", Uuid::new_v4()));
    let mut config = Config::load_from_dir(dir).unwrap();
    let id = format!("phone-{}", Uuid::new_v4().simple());
    let info = DeviceInfo::local(id.clone(), "Pixel".into());
    config.trust_device(&info, "cert-1".into()).unwrap();
    config.stored.settings.walk_away_devices.insert(id.clone());
    config.trust_device(&info, "cert-1".into()).unwrap();
    assert!(config.stored.settings.locks_when_away(&id), "same identity");
    assert!(
        !config.stored.settings.locks_when_away("other"),
        "per phone"
    );
    config.trust_device(&info, "cert-2".into()).unwrap();
    assert!(!config.stored.settings.locks_when_away(&id), "new identity");
    config.stored.settings.walk_away_devices.insert(id.clone());
    config.untrust_device(&id).unwrap();
    assert!(!config.stored.settings.locks_when_away(&id), "unpaired");
}

/// F7: files waiting for a phone are for that identity: a new pairing
/// certificate behind the same id drops them, the same one keeps them.
#[test]
fn a_new_certificate_drops_the_files_waiting_for_the_phone() {
    use crate::device_links::daemon::file_transfer as queue;
    let dir = std::env::temp_dir().join(format!("desklink-waitcert-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("f.txt");
    std::fs::write(&file, "x").unwrap();
    let mut config = Config::load_from_dir(dir.clone()).unwrap();
    let id = format!("phone-{}", Uuid::new_v4().simple());
    let info = DeviceInfo::local(id.clone(), "Pixel".into());
    config.trust_device(&info, "cert-1".into()).unwrap();
    queue::wait_for_phone_for_tests(&id, "Pixel", file);
    config.trust_device(&info, "cert-1".into()).unwrap();
    assert_eq!(
        queue::waiting_files(&id).len(),
        1,
        "same identity keeps them"
    );
    config.trust_device(&info, "cert-2".into()).unwrap();
    assert!(
        queue::waiting_files(&id).is_empty(),
        "a new identity does not"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Photos from a phone and Do Not Disturb sharing are off until turned on
/// per phone, stay out of the file while off, and go with the pairing and
/// with the identity.
#[test]
fn gallery_and_dnd_sharing_are_per_phone_off_by_default_and_do_not_survive_unpair() {
    let old: AppSettings = serde_json::from_str(r#"{"stay_awake":true}"#).unwrap();
    assert!(!old.accepts_gallery_from("phone") && !old.shares_dnd_with("phone"));
    let written = serde_json::to_string(&AppSettings::default()).unwrap();
    assert!(!written.contains("gallery_devices") && !written.contains("dnd_devices"));

    let dir = std::env::temp_dir().join(format!("desklink-gallery-{}", Uuid::new_v4()));
    let mut config = Config::load_from_dir(dir).unwrap();
    let id = format!("phone-{}", Uuid::new_v4().simple());
    let info = DeviceInfo::local(id.clone(), "Pixel".into());
    config.trust_device(&info, "cert-1".into()).unwrap();
    let on = |config: &mut Config| {
        config.stored.settings.gallery_devices.insert(id.clone());
        config.stored.settings.dnd_devices.insert(id.clone());
    };
    on(&mut config);
    config.trust_device(&info, "cert-1".into()).unwrap();
    assert!(config.stored.settings.accepts_gallery_from(&id));
    assert!(config.stored.settings.shares_dnd_with(&id));
    assert!(!config.stored.settings.accepts_gallery_from("other"));
    config.trust_device(&info, "cert-2".into()).unwrap();
    assert!(
        !config.stored.settings.accepts_gallery_from(&id),
        "new identity"
    );
    assert!(!config.stored.settings.shares_dnd_with(&id), "new identity");
    on(&mut config);
    config.untrust_device(&id).unwrap();
    assert!(
        !config.stored.settings.accepts_gallery_from(&id),
        "unpaired"
    );
    assert!(!config.stored.settings.shares_dnd_with(&id), "unpaired");
}

/// "Unlock with this phone" is gone with the pairing, and with the
/// identity: a different certificate behind the same id starts from off.
#[test]
fn unlock_with_a_phone_does_not_survive_unpair_or_a_new_identity() {
    let dir = std::env::temp_dir().join(format!("desklink-unlock-{}", Uuid::new_v4()));
    let mut config = Config::load_from_dir(dir.clone()).unwrap();
    let id = format!("phone-{}", Uuid::new_v4().simple());
    let info = DeviceInfo::local(id.clone(), "Pixel".into());
    config.trust_device(&info, "cert-1".into()).unwrap();
    config.stored.settings.unlock_devices.insert(id.clone());
    config.trust_device(&info, "cert-1".into()).unwrap();
    assert!(
        config.stored.settings.unlocks_from(&id),
        "same identity keeps it"
    );
    config.trust_device(&info, "cert-2".into()).unwrap();
    assert!(
        !config.stored.settings.unlocks_from(&id),
        "a new identity starts from off"
    );
    config.stored.settings.unlock_devices.insert(id.clone());
    config
        .stored
        .settings
        .hidden_notifications
        .insert(id.clone());
    config
        .stored
        .settings
        .muted_command_alerts
        .insert(id.clone());
    config
        .stored
        .settings
        .contact_caller_names
        .insert(id.clone());
    config.untrust_device(&id).unwrap();
    assert!(!config.stored.settings.unlocks_from(&id), "unpaired");
    assert!(config.stored.settings.shows_notifications(&id));
    assert!(config.stored.settings.alerts_commands_to(&id));
    assert!(!config.stored.settings.contact_caller_names.contains(&id));
    let _ = fs::remove_dir_all(dir);
}

/// Computing where transfer state lives is pure; only `prepare` moves it.
#[test]
fn transfer_state_moves_only_when_prepared() {
    let base = std::env::temp_dir().join(format!("desklink-statepaths-{}", Uuid::new_v4()));
    let old = base.join("old");
    let new = base.join("new");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("a.part"), "x").unwrap();
    let config = Config::load_from_dir(base.join("cfg")).unwrap();
    let same = config.transfer_state_paths();
    assert_eq!(same.old, same.new, "a test root keeps its place");
    let paths = TransferStatePaths {
        old: old.clone(),
        new: new.clone(),
    };
    assert!(old.join("a.part").exists() && !new.exists(), "nothing yet");
    assert_eq!(paths.prepare(), new);
    assert!(new.join("a.part").exists() && !old.exists());
    let _ = fs::remove_dir_all(base);
}

/// A new identity behind the same id does not inherit what the old one
/// left in memory (here the glide report and lane back-off).
#[test]
fn a_new_certificate_forgets_the_old_ones_session_state() {
    let dir = std::env::temp_dir().join(format!("desklink-idstate-{}", Uuid::new_v4()));
    let mut config = Config::load_from_dir(dir.clone()).unwrap();
    let id = format!("phone-{}", Uuid::new_v4().simple());
    let info = DeviceInfo::local(id.clone(), "Pixel".into());
    config.trust_device(&info, "cert-1".into()).unwrap();
    let mut allowed = crate::device_links::packet::NetworkPacket::new("desklink.shareinputdevices");
    allowed.set("allowed", true);
    crate::device_links::glide::on_phone_packet(&id, &allowed);
    crate::device_links::webrtc::file_lane::penalize_peer(
        &id,
        std::time::Duration::from_secs(10),
        std::time::Instant::now(),
    );
    config.trust_device(&info, "cert-1".into()).unwrap();
    assert_eq!(crate::device_links::glide::phone_allows(&id), Some(true));
    config.trust_device(&info, "cert-2".into()).unwrap();
    assert_eq!(crate::device_links::glide::phone_allows(&id), None);
    assert!(!crate::device_links::webrtc::file_lane::peer_blocked(
        &id,
        std::time::Instant::now()
    ));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_poisoned_settings_mirror_still_answers_with_what_it_held() {
    let before = app_settings();
    let poisoner = std::thread::spawn(|| {
        let _held = settings_mirror().lock().unwrap();
        panic!("poisoning the settings mirror on purpose");
    });
    assert!(poisoner.join().is_err());
    assert!(settings_mirror().lock().is_err(), "the mirror is poisoned");
    // Defaults here would read as "no session password" and open the gate it guards.
    assert_eq!(app_settings(), before);
    settings_mirror().clear_poison(); // other tests share the mirror
}
