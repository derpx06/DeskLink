//! The vectors both apps must agree on, read from the files shared with the
//! phone (`protocol/testdata/desklink_v9/`, copied byte for byte to
//! `testdata/`; `protocol/check-fixtures.sh` compares every copy). Each test
//! computes with the desk's own code: a mismatch is a real cross-app bug,
//! not a test to adjust.

use serde_json::Value;

fn load(text: &str) -> Value {
    serde_json::from_str(text).expect("a fixture is valid JSON")
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("a list")
        .iter()
        .map(|item| item.as_str().expect("a string").to_string())
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn byte_filled(value: &Value, key: &str) -> u8 {
    value[key].as_u64().unwrap_or_else(|| panic!("{key}")) as u8
}

#[test]
fn the_qr_proof_is_the_phones() {
    let f = load(include_str!("../testdata/qr_proof.json"));
    let proof = crate::device_links::pairing_qr::proof(
        &[byte_filled(&f, "secretByte"); 32],
        f["desk"].as_str().unwrap(),
        f["phone"].as_str().unwrap(),
        &[byte_filled(&f, "nonceDeskByte"); 32],
        &[byte_filled(&f, "noncePhoneByte"); 32],
    )
    .unwrap();
    assert_eq!(hex(&proof), f["proofHex"].as_str().unwrap());
}

#[test]
fn the_relay_end_to_end_vector_is_the_phones() {
    use crate::device_links::relay_link::crypto::{
        derive_keys, seal, transcript, Ephemeral, Role, Transcript,
    };
    use openssl::hash::{hash, MessageDigest};
    let f = load(include_str!("../testdata/relay_e2e.json"));
    let nonce = |key: &str| [byte_filled(&f, key); 32];
    let (nonce_i, nonce_r) = (nonce("nonceInitiatorByte"), nonce("nonceResponderByte"));
    let i = Ephemeral::from_scalar(&[byte_filled(&f, "scalarInitiatorByte"); 32], nonce_i).unwrap();
    let r = Ephemeral::from_scalar(&[byte_filled(&f, "scalarResponderByte"); 32], nonce_r).unwrap();
    let (fp_i, fp_r) = (
        f["fpInitiator"].as_str().unwrap(),
        f["fpResponder"].as_str().unwrap(),
    );
    assert_eq!(
        hex(&i.public[..9]),
        f["initiatorPublicPrefixHex"].as_str().unwrap()
    );
    let on_i = derive_keys(
        &i,
        &r.public,
        Role::Initiator,
        fp_i,
        fp_r,
        &nonce_i,
        &nonce_r,
    )
    .unwrap();
    let on_r = derive_keys(
        &r,
        &i.public,
        Role::Responder,
        fp_i,
        fp_r,
        &nonce_i,
        &nonce_r,
    )
    .unwrap();
    assert_eq!(on_i.send, on_r.receive);
    assert_eq!(on_i.receive, on_r.send);
    assert_eq!(hex(&on_i.send), f["initiatorSendKeyHex"].as_str().unwrap());
    assert_eq!(
        hex(&on_i.receive),
        f["initiatorReceiveKeyHex"].as_str().unwrap()
    );
    let bytes = transcript(&Transcript {
        fp_initiator: fp_i,
        fp_responder: fp_r,
        eph_initiator: &i.public,
        eph_responder: &r.public,
        nonce_initiator: &nonce_i,
        nonce_responder: &nonce_r,
        identity_initiator: f["identityInitiator"].as_str().unwrap(),
        identity_responder: f["identityResponder"].as_str().unwrap(),
    });
    assert_eq!(
        hex(&hash(MessageDigest::sha256(), &bytes).unwrap()),
        f["transcriptSha256Hex"].as_str().unwrap()
    );
    assert_eq!(
        seal(&on_i.send, 1, f["plaintext"].as_str().unwrap().as_bytes()).unwrap(),
        f["sealedSeq1"].as_str().unwrap()
    );
}

#[test]
fn the_relay_auth_sign_input_is_the_phones() {
    let f = load(include_str!("../testdata/auth_sign_input.json"));
    let bytes = desklink_relay::protocol::auth_sign_input(
        f["nonce"].as_str().unwrap(),
        f["host"].as_str().unwrap(),
    );
    assert_eq!(hex(&bytes), f["hex"].as_str().unwrap());
}

/// Winners of one vector, in input order.
fn winners_of(names: &Value) -> Vec<String> {
    let input = strings(names);
    let mut items: std::collections::BTreeMap<String, ()> =
        input.iter().cloned().map(|name| (name, ())).collect();
    crate::device_links::folders::drop_case_clashes_for_tests(&mut items);
    input
        .into_iter()
        .filter(|name| items.contains_key(name))
        .collect()
}

#[test]
fn the_case_clash_winners_are_the_phones() {
    let f = load(include_str!("../testdata/case_clash.json"));
    assert_eq!(
        winners_of(&f["names"]),
        strings(&f["winners"]),
        "the original vector"
    );
    let cases = f["cases"].as_array().map_or(&[][..], Vec::as_slice);
    for case in cases {
        assert_eq!(
            winners_of(&case["names"]),
            strings(&case["winners"]),
            "{}",
            case["name"]
        );
    }
}

#[test]
fn the_desk_case_clash_vectors_fold_by_the_agreed_rule() {
    let f = load(include_str!("../testdata/case_clash_desk.json"));
    let mut items: std::collections::BTreeMap<String, ()> = strings(&f["names"])
        .into_iter()
        .map(|name| (name, ()))
        .collect();
    crate::device_links::folders::drop_case_clashes_for_tests(&mut items);
    let mut ours: Vec<String> = items.into_keys().collect();
    let mut theirs = strings(&f["winners"]);
    ours.sort();
    theirs.sort();
    assert_eq!(ours, theirs);
    // The rule itself, spelled out.
    use crate::device_links::folders::fold_name;
    assert_eq!(fold_name("A.\u{3a3}"), fold_name("a.\u{3c3}"), "Σ is σ");
    assert_ne!(fold_name("\u{3a3}"), "\u{3c2}", "never the final sigma");
    assert_eq!(fold_name("\u{130}"), "\u{130}", "İ has no single lowercase");
    assert_eq!(fold_name("\u{df}"), "\u{df}");
    assert_eq!(fold_name("\u{10400}"), "\u{10428}");
    assert_ne!(
        fold_name("\u{e9}"),
        fold_name("e\u{301}"),
        "no normalisation"
    );
}

#[test]
fn the_webrtc_profile_is_the_phones_and_the_removed_types_stay_out() {
    let f = load(include_str!("../testdata/profile.json"));
    let mut ours: Vec<String> = crate::device_links::features::webrtc_profile()
        .iter()
        .map(|name| name.to_string())
        .collect();
    ours.sort();
    assert_eq!(ours.len(), 32);
    assert_eq!(ours, strings(&f["desk"]), "the desk's profile");
    assert_eq!(ours, strings(&f["phone"]), "and the phone's, equal today");

    let removed = load(include_str!("../testdata/removed_types.json"));
    let mut theirs = strings(&removed["types"]);
    let mut ours: Vec<String> = crate::device_links::features::REMOVED_TYPES
        .iter()
        .map(|name| name.to_string())
        .collect();
    ours.sort();
    theirs.sort();
    assert_eq!(ours, theirs, "the removed types");
    for name in &ours {
        assert!(
            !crate::device_links::features::webrtc_profile().contains(&name.as_str()),
            "{name} is removed but in the profile"
        );
    }
}

// ── Fixtures the desk wrote for the phone to read ──────────────────────────

fn packet_from(body: &Value) -> crate::device_links::packet::NetworkPacket {
    let mut packet = crate::device_links::packet::NetworkPacket::new("desklink.test");
    for (key, value) in body.as_object().expect("a body") {
        packet.body.insert(key.clone(), value.clone());
    }
    packet
}

#[test]
fn glide_bodies_are_classified_as_written() {
    use crate::device_links::glide::{classify_phone_packet, PhoneReport};
    let f = load(include_str!("../testdata/glide_bodies.json"));
    for case in f["cases"].as_array().unwrap() {
        let expected = match case["report"].as_str().unwrap() {
            "state" => PhoneReport::State {
                allowed: case["allowed"].as_bool().unwrap(),
                entered: case["entered"].as_bool().unwrap(),
            },
            "handedBack" => PhoneReport::HandedBack,
            "release" => PhoneReport::Release,
            _ => PhoneReport::Ignored,
        };
        assert_eq!(
            classify_phone_packet(&packet_from(&case["body"])),
            expected,
            "{}",
            case["name"]
        );
    }
}

#[test]
fn screen_requests_are_served_as_written() {
    use crate::device_links::webrtc::screen::ScreenRequest;
    let f = load(include_str!("../testdata/control_vectors.json"));
    let defaults = ScreenRequest::from_body(&packet_from(&serde_json::json!({})));
    assert_eq!(
        i64::from(defaults.max_dimension),
        f["defaults"]["maxDimension"]
    );
    assert_eq!(i64::from(defaults.fps), f["defaults"]["fps"]);
    for case in f["cases"].as_array().unwrap() {
        let served = ScreenRequest::from_body(&packet_from(&case["body"]));
        assert_eq!(
            (i64::from(served.max_dimension), i64::from(served.fps)),
            (
                case["maxDimension"].as_i64().unwrap(),
                case["fps"].as_i64().unwrap()
            ),
            "{}",
            case["name"]
        );
    }
}

#[test]
fn notes_merge_as_written() {
    let f = load(include_str!("../testdata/notes_merge.json"));
    for case in f["cases"].as_array().unwrap() {
        let outcome =
            crate::device_links::notes::merge_for_fixture(&case["local"], &case["incoming"]);
        assert_eq!(
            outcome,
            case["outcome"].as_str().unwrap(),
            "{}",
            case["name"]
        );
    }
}

#[test]
fn vcards_read_as_written() {
    let f = load(include_str!("../testdata/contacts_bodies.json"));
    for case in f["cases"].as_array().unwrap() {
        let contact = crate::device_links::contacts::parse_vcard(
            case["uid"].as_str().unwrap(),
            case["card"].as_str().unwrap(),
        )
        .unwrap_or_else(|| panic!("{} did not parse", case["case"]));
        assert_eq!(
            contact.name,
            case["name"].as_str().unwrap(),
            "{}",
            case["case"]
        );
        let phones: Vec<(String, String)> = contact
            .phones
            .iter()
            .map(|phone| (phone.kind.clone(), phone.number.clone()))
            .collect();
        let expected: Vec<(String, String)> = case["phones"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| {
                (
                    pair[0].as_str().unwrap().to_string(),
                    pair[1].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(phones, expected, "{}", case["case"]);
        assert_eq!(contact.emails, strings(&case["emails"]), "{}", case["case"]);
    }
}
