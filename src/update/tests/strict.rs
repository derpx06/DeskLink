//! The strict profile and the rules from the signing-side review: JSON,
//! base64url, Ed25519, time windows, links, components, text, key sets, and a
//! deterministic byte-flip fuzz.

use super::*;
use crate::update::model::*;
use crate::update::strict_json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B;

fn payload(component: &str, issued: u64, expires: u64) -> String {
    format!(
        r#"{{"schemaVersion":1,"issuedAt":{issued},"expires":{expires},"channels":{{"beta":{{"desktop":{component}}}}}}}"#
    )
}

fn good(component: &str) -> String {
    payload(component, NOW, NOW + 86_400)
}

fn run(payload: &str, last: u64) -> Result<Payload, Reject> {
    verify(&sign(payload), &keys(), NOW, last)
}

fn beta(component: &str) -> Option<Component> {
    run(&good(component), 0).unwrap().beta
}

#[test]
fn the_json_profile() {
    let ok = |t: &str| strict_json::parse(t.as_bytes()).is_some();
    assert!(ok(r#"{"a":[1,2,{"b":null}],"c":"d\u00e9"}"#));
    assert!(ok(" \t\r\n{ \"a\" : [ ] , \"b\" : { } }\n"));
    for bad in [
        r#"{"a":1,"a":2}"#,
        r#"{"a":{"b":1,"b":1}}"#,
        r#"{"a":1,"\u0061":2}"#,
        "NaN",
        "Infinity",
        r#"{"a":NaN}"#,
        "\u{feff}{}",
        "{} {}",
        "{}x",
        r#"{"a":"\ud800"}"#,
        r#"{"a":"\udc00x"}"#,
        r#"{"a":"\ud800\u0041"}"#,
        "{'a':1}",
        "",
        r#"{"a":1,}"#,
        r#"[1,]"#,
        r#"{a:1}"#,
        "{\"a\":1}/*c*/",
        "// c\n{}",
        // Numbers: the whole-document errors.
        r#"{"a":-1}"#,
        r#"{"a":-0}"#,
        r#"{"a":+1}"#,
        r#"{"a":1.0}"#,
        r#"{"a":100.0}"#,
        r#"{"a":.5}"#,
        r#"{"a":5e1}"#,
        r#"{"a":1E2}"#,
        r#"{"a":1e999}"#,
        r#"{"a":01}"#,
        r#"{"a":00}"#,
        "{\"a\":\"x\ny\"}",
        "{\"a\":\"x\u{1}y\"}",
        "{\u{a0}}",
    ] {
        assert!(!ok(bad), "{bad:?}");
    }
    assert!(strict_json::parse(b"{\"a\":\"\xff\"}").is_none());
    assert!(
        strict_json::parse(&[0xff, 0xfe, b'{', 0, b'}', 0]).is_none(),
        "UTF-16"
    );
    // An integer past i64 passes the grammar and reads as out of range.
    let big =
        strict_json::parse(br#"{"a":9223372036854775808,"b":99999999999999999999999}"#).unwrap();
    assert_eq!(big["a"].as_i64(), None);
    assert_eq!(big["b"].as_i64(), None);
    // Depth: the top-level value is depth 1; 32 is accepted, 33 is not.
    let nest = |n: usize| format!("{}1{}", "[".repeat(n), "]".repeat(n));
    assert!(ok(&nest(32)) && !ok(&nest(33)));
    let mixed = |n: usize| {
        let mut text = String::new();
        for i in 0..n {
            text.push_str(if i % 2 == 0 { r#"{"a":"# } else { "[" });
        }
        text.push('1');
        for i in (0..n).rev() {
            text.push(if i % 2 == 0 { '}' } else { ']' });
        }
        text
    };
    assert!(ok(&mixed(32)) && !ok(&mixed(33)));
    assert!(
        ok(r#"{"a":"[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[["}"#),
        "brackets in strings do not count"
    );
    assert!(
        !ok(&nest(32_000)),
        "a very deep body is an error, not a stack overflow"
    );
}

#[test]
fn the_envelope_is_exactly_four_keys_with_the_right_types() {
    let valid = sign(&good(r#"{"version":"0.4.0"}"#));
    assert!(verify(&valid, &keys(), NOW, 0).is_ok());
    let env: serde_json::Value = serde_json::from_slice(&valid).unwrap();
    let with = |mutate: &dyn Fn(&mut serde_json::Value)| {
        let mut e = env.clone();
        mutate(&mut e);
        verify(e.to_string().as_bytes(), &keys(), NOW, 0)
    };
    assert_eq!(with(&|e| e["extra"] = 1.into()), Err(Reject::Parse));
    assert_eq!(with(&|e| e["v"] = "1".into()), Err(Reject::Parse));
    assert_eq!(with(&|e| e["v"] = true.into()), Err(Reject::Parse));
    assert_eq!(
        with(&|e| e["v"] = serde_json::json!(1.0)),
        Err(Reject::Parse)
    );
    assert_eq!(with(&|e| e["v"] = 2.into()), Err(Reject::Parse));
    assert_eq!(with(&|e| e["kid"] = 5.into()), Err(Reject::Parse));
    assert_eq!(
        with(&|e| {
            e.as_object_mut().unwrap().remove("sig");
        }),
        Err(Reject::Parse)
    );
    assert_eq!(with(&|e| e["kid"] = "nope".into()), Err(Reject::Signature));
    // The order: shape, then the key id, then payload and sig.
    assert_eq!(
        with(&|e| {
            e["kid"] = "nope".into();
            e["payload"] = 5.into();
        }),
        Err(Reject::Signature),
        "unknown kid before payload"
    );
    assert_eq!(
        with(&|e| e["payload"] = 5.into()),
        Err(Reject::Signature),
        "known kid, payload not a string"
    );
    assert_eq!(
        with(&|e| e["sig"] = serde_json::Value::Null),
        Err(Reject::Signature)
    );
    assert_eq!(
        with(&|e| {
            e["kid"] = 5.into();
            e["payload"] = 5.into();
        }),
        Err(Reject::Parse),
        "a kid that is not a string is a shape error"
    );
    assert_eq!(
        with(&|e| e["payload"] = "".into()),
        Err(Reject::Signature),
        "an empty payload fails its signature"
    );
    // Duplicate keys in the envelope text.
    let text = String::from_utf8(valid.clone())
        .unwrap()
        .replacen('{', r#"{"v":1,"#, 1);
    assert_eq!(verify(text.as_bytes(), &keys(), NOW, 0), Err(Reject::Parse));
}

#[test]
fn base64url_must_be_canonical() {
    let kid = json("keys.json")["keys"][0]["kid"]
        .as_str()
        .unwrap()
        .to_string();
    let p = good(r#"{"version":"0.4.0"}"#);
    let valid = sign(&p);
    let env: serde_json::Value = serde_json::from_slice(&valid).unwrap();
    let (pb, sb) = (
        env["payload"].as_str().unwrap().to_string(),
        env["sig"].as_str().unwrap().to_string(),
    );
    // Padding.
    let padded = base64::engine::general_purpose::URL_SAFE.encode(p.as_bytes());
    if padded.ends_with('=') {
        assert_eq!(
            verify(&envelope("1", &kid, &padded, &sb), &keys(), NOW, 0),
            Err(Reject::Parse)
        );
    }
    // The standard alphabet.
    let std = base64::engine::general_purpose::STANDARD_NO_PAD.encode(p.as_bytes());
    if std.contains('+') || std.contains('/') {
        assert!(verify(&envelope("1", &kid, &std, &sb), &keys(), NOW, 0).is_err());
    }
    // Non-zero trailing bits in the last character of the signature.
    assert_eq!(sb.len(), 86);
    let last = sb.chars().last().unwrap();
    let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let at = alphabet.find(last).unwrap();
    let other = alphabet.chars().nth(at ^ 1).unwrap();
    let tweaked = format!("{}{}", &sb[..85], other);
    // The decoder itself already refuses non-zero trailing bits; the re-encode
    // comparison in `b64url` is the second line.
    assert!(B.decode(&tweaked).is_err());
    assert_eq!(
        verify(&envelope("1", &kid, &pb, &tweaked), &keys(), NOW, 0),
        Err(Reject::Signature)
    );
    // Whitespace inside.
    assert!(verify(
        &envelope("1", &kid, &format!("{pb}\\n"), &sb),
        &keys(),
        NOW,
        0
    )
    .is_err());
}

/// `S` plus `L` is the same signature to a lax verifier; this one refuses it.
#[test]
fn a_signature_with_s_at_or_above_l_is_refused() {
    let valid = sign(&good(r#"{"version":"0.4.0"}"#));
    let env: serde_json::Value = serde_json::from_slice(&valid).unwrap();
    let mut sig = B.decode(env["sig"].as_str().unwrap()).unwrap();
    let l: [u8; 32] = [
        0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde,
        0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
    ];
    let mut carry = 0u16;
    for (byte, add) in sig[32..].iter_mut().zip(l) {
        let sum = u16::from(*byte) + u16::from(add) + carry;
        *byte = sum as u8;
        carry = sum >> 8;
    }
    assert_eq!(carry, 0, "S + L fits in 256 bits");
    let mut high = env.clone();
    high["sig"] = B.encode(&sig).into();
    assert_eq!(
        verify(high.to_string().as_bytes(), &keys(), NOW, 0),
        Err(Reject::Signature)
    );
}

#[test]
fn the_size_edge() {
    let at = |n: usize| {
        let mut body = sign(&good("null"));
        // Pad inside the JSON whitespace so the document stays one envelope.
        body.splice(
            body.len()..body.len(),
            std::iter::repeat_n(b' ', n.saturating_sub(body.len())),
        );
        body
    };
    assert_ne!(verify(&at(65_536), &keys(), NOW, 0), Err(Reject::Oversized));
    assert_eq!(verify(&at(65_537), &keys(), NOW, 0), Err(Reject::Oversized));
}

#[test]
fn the_time_windows() {
    let check = |issued: u64, expires: u64| run(&payload("null", issued, expires), 0).map(|_| ());
    assert!(
        check(NOW + 3_600, NOW + 86_400).is_ok(),
        "an hour ahead is tolerated"
    );
    assert_eq!(check(NOW + 3_601, NOW + 86_400), Err(Reject::Expired));
    assert!(
        check(NOW - 22 * 86_400, NOW).is_ok(),
        "22 days of validity, expiring now"
    );
    assert_eq!(
        check(NOW - 22 * 86_400 - 1, NOW),
        Err(Reject::Expired),
        "23 days of validity"
    );
    assert!(check(NOW, NOW + 22 * 86_400).is_ok());
    assert_eq!(
        check(NOW, NOW + 22 * 86_400 + 1),
        Err(Reject::Expired),
        "signed to outlive the 22 days"
    );
    assert_eq!(check(NOW - 100, NOW - 1), Err(Reject::Expired));
    assert_eq!(check(NOW, NOW), Err(Reject::Expired), "a validity of zero");
    assert_eq!(
        check(NOW + 5, NOW + 4),
        Err(Reject::Expired),
        "expires before it was issued"
    );
    // Replay: equal is fine, older is not.
    assert!(run(&payload("null", NOW, NOW + 100), NOW).is_ok());
    assert_eq!(
        run(&payload("null", NOW - 1, NOW + 100), NOW),
        Err(Reject::Replay)
    );
}

#[test]
fn the_newest_seen_time_never_runs_ahead_of_the_clock() {
    use crate::update::engine::Engine;
    let fake = Fake::default();
    let mut engine = Engine::new(
        Some(fake.clone()),
        keys(),
        v("0.3.0"),
        Some(scratch("min")),
        NOW,
    );
    // Issued 50 minutes ahead (within the tolerance).
    fake.body(
        sign(&payload(
            r#"{"version":"0.4.0"}"#,
            NOW + 3_000,
            NOW + 86_400,
        )),
        "\"a\"",
    );
    engine.check(&settings(), NOW, false);
    assert_eq!(engine.store.last_issued_at, NOW, "min(issuedAt, now)");
    // So an honest manifest issued a moment later is not a replay.
    fake.body(
        sign(&payload(r#"{"version":"0.4.0"}"#, NOW + 10, NOW + 86_400)),
        "\"b\"",
    );
    engine.check(&settings(), NOW + 20, true);
    assert_eq!(engine.store.last_issued_at, NOW + 10);
}

#[test]
fn links_are_exact_shapes_for_this_release() {
    let page = |u: &str| url_ok(u, LinkField::ReleasePage, &v("0.4.0"));
    let tar = |u: &str| url_ok(u, LinkField::Tarball, &v("0.4.0"));
    assert!(page(
        "https://github.com/derpx06/DeskLink/releases/tag/v0.4.0"
    ));
    assert!(page(
        "HTTPS://GitHub.COM/derpx06/DeskLink/releases/tag/v0.4.0"
    ));
    assert!(page(
        "https://github.com:443/derpx06/DeskLink/releases/tag/v0.4.0"
    ));
    assert!(tar(
        "https://github.com/derpx06/DeskLink/releases/download/v0.4.0/desklink-0.4.0-x86_64.tar.gz"
    ));
    for bad in [
        "https://github.com/derpx06/DeskLink/releases/tag/v0.4.1",
        "https://github.com/derpx06/DeskLink/releases/tag/v0.4.0/",
        "https://github.com/derpx06/DeskLink/releases/tag/0.4.0",
        "https://github.com/derpx06/DeskLink/releases/tag/v00.4.0",
        "https://github.com/derpx06/DeskLink/releases/latest",
        "https://github.com/derpx06/DeskLink/releases/tag/v0.4.0?x=1",
        "https://github.com/derpx06/DeskLink/releases/tag/v0.4.0#f",
        "https://github.com/derpx06/DeskLink/releases/tag/v0.4.0%2f",
        "https://github.com/derpx06/DeskLink/releases/tag/v0.4.0\\x",
        "https://github.com/derpx06/DeskLink/releases/tag/v0.4.0 ",
        " https://github.com/derpx06/DeskLink/releases/tag/v0.4.0",
        "https://github.com/derpx06/DeskLink/releases/tag/v0.4.0\n",
        "https://github.com/derpx06/DeskLink/releases/tag/v0.4.\u{e9}",
        "https://github.com//derpx06/DeskLink/releases/tag/v0.4.0",
        "https://github.com/derpx06/DeskLink/releases/./tag/v0.4.0",
        "https://github.com/derpx06/DeskLink/releases/tag/../tag/v0.4.0",
        "https://github.com./derpx06/DeskLink/releases/tag/v0.4.0",
        "https://user@github.com/derpx06/DeskLink/releases/tag/v0.4.0",
        "https://github.com:8443/derpx06/DeskLink/releases/tag/v0.4.0",
        "https://github.com:/derpx06/DeskLink/releases/tag/v0.4.0",
        "http://github.com/derpx06/DeskLink/releases/tag/v0.4.0",
        "https://github.com/derpx06/desklink-mobile/releases/tag/v0.4.0",
        "https://github.com/evil/DeskLink/releases/tag/v0.4.0",
        "https://github.com/derpx06/DeskLinkX/releases/tag/v0.4.0",
        "https://github.com.evil.example/derpx06/DeskLink/releases/tag/v0.4.0",
        "https://objects.githubusercontent.com/derpx06/DeskLink/releases/tag/v0.4.0",
        "https://play.google.com/store/apps/details?id=org.desklink.mobile",
        "https://github.com/derpx06/DeskLink/releases/download/v0.4.0/desklink-0.4.0-x86_64.tar.gz",
        "javascript:alert(1)",
        "",
    ] {
        assert!(!page(bad), "{bad:?}");
    }
    for bad in [
        "https://github.com/derpx06/DeskLink/releases/download/v0.4.1/desklink-0.4.1-x86_64.tar.gz",
        "https://github.com/derpx06/DeskLink/releases/download/v0.4.0/desklink-0.4.1-x86_64.tar.gz",
        "https://github.com/derpx06/DeskLink/releases/download/v0.4.0/evil.tar.gz",
        "https://github.com/derpx06/DeskLink/releases/download/v0.4.0/desklink-0.4.0.apk",
        "https://github.com/derpx06/desklink-mobile/releases/download/v0.4.0/desklink-0.4.0-x86_64.tar.gz",
    ] {
        assert!(!tar(bad), "{bad:?}");
    }
}

#[test]
fn a_malformed_component_is_dropped_and_the_other_still_decides() {
    for bad in [
        r#"{"version":"0.4.0","rollout":101}"#,
        r#"{"version":"0.4.0","rollout":"50"}"#,
        r#"{"version":"0.4.0","rollout":null}"#,
        r#"{"version":"0.4.0","rollout":true}"#,
        r#"{"version":"01.4.0"}"#,
        r#"{"version":"0.4.0+build"}"#,
        r#"{"version":"0.4.0-rc1"}"#,
        r#"{"version":"0.4.0.1"}"#,
        r#"{"version":"0.4"}"#,
        r#"{"version":"99999999999.0.0"}"#,
        "{\"version\":\"\u{663}.0.0\"}",
        r#"{"version":" 0.4.0"}"#,
        r#"{"version":4}"#,
        r#"{"version":"0.4.0","minSupported":"x.y.z"}"#,
        r#"{"version":"0.4.0","minSupported":"0.03.0"}"#,
        r#"{"version":"0.4.0","minSupported":"0.5.0"}"#,
        "[]",
        r#""0.4.0""#,
        "5",
    ] {
        assert!(beta(bad).is_none(), "{bad}");
    }
    // Beta is bad, stable is fine: stable is what a beta install decides on.
    let both = format!(
        r#"{{"schemaVersion":1,"issuedAt":{NOW},"expires":{},"channels":{{"beta":{{"desktop":{{"version":"0.4","rollout":999}}}},"stable":{{"desktop":{{"version":"1.0.0"}}}}}}}}"#,
        NOW + 1000
    );
    let p = run(&both, 0).unwrap();
    assert!(p.beta.is_none());
    let d = decide(
        v("0.3.0"),
        Channel::Beta,
        &p,
        &Prefs::default(),
        &[1; 16],
        NOW,
        false,
    );
    assert!(matches!(d, Decision::Available(c) if c.version == v("1.0.0")));
    // Both bad: nothing to offer, not an error.
    let none = payload(r#"{"version":"x"}"#, NOW, NOW + 1000);
    let p = run(&none, 0).unwrap();
    assert_eq!(
        decide(
            v("0.3.0"),
            Channel::Beta,
            &p,
            &Prefs::default(),
            &[1; 16],
            NOW,
            false
        ),
        Decision::UpToDate
    );
}

#[test]
fn rollout_zero_pauses_everything_and_the_percentages_do_not_bind_manual_or_required() {
    let id = [3u8; 16];
    let make = |rollout: u8, min: &str| Payload {
        issued_at: 1,
        expires: u64::MAX,
        beta: Some(Component {
            version: v("0.4.0"),
            released: None,
            min_supported: v(min),
            required_reason: None,
            rollout,
            notes: vec![],
            notes_url: None,
            release_url: None,
            tarball: None,
        }),
        stable: None,
    };
    let b = bucket(&id, "desktop", &v("0.4.0")) as u8;
    let run = |p: &Payload, manual| {
        verdict(&decide(
            v("0.3.0"),
            Channel::Beta,
            p,
            &Prefs::default(),
            &id,
            NOW,
            manual,
        ))
    };
    for manual in [false, true] {
        assert_eq!(
            run(&make(0, "0.0.0"), manual),
            "UpToDate",
            "paused (manual {manual})"
        );
        assert_eq!(
            run(&make(0, "0.3.5"), manual),
            "UpToDate",
            "a paused Required is paused too"
        );
    }
    if b > 0 {
        assert_eq!(run(&make(b, "0.0.0"), false), "UpToDate");
        assert_eq!(
            run(&make(b, "0.0.0"), true),
            "Offer:0.4.0",
            "by hand ignores the percentage"
        );
        assert_eq!(
            run(&make(b, "0.3.5"), false),
            "Required:0.4.0",
            "Required ignores it too"
        );
    }
}

fn verdict(d: &Decision) -> String {
    match d {
        Decision::UpToDate => "UpToDate".into(),
        Decision::Available(c) => format!("Offer:{}", c.version),
        Decision::Required(c) => format!("Required:{}", c.version),
        Decision::Skipped(c) => format!("Skipped:{}", c.version),
        Decision::Snoozed(c, _) => format!("Snoozed:{}", c.version),
    }
}

#[test]
fn rotation_a_key_set_without_the_old_key_refuses_its_manifests() {
    let all = keys();
    let signed_by_first = fixture("manifests/valid-beta.json");
    assert!(verify(&signed_by_first, &all, NOW, 0).is_ok());
    let next_only: Vec<Key> = all.iter().skip(1).cloned().collect();
    assert!(!next_only.is_empty());
    assert_eq!(
        verify(&signed_by_first, &next_only, NOW, 0),
        Err(Reject::Signature)
    );
    // A key that is not in the set at all is refused the same way.
    assert_eq!(
        verify(&signed_by_first, &[], NOW, 0),
        Err(Reject::Signature)
    );
}

/// Deterministic byte flips, cuts and splices over every fixture: no input
/// may panic, and none that verified before a flip may verify as something
/// that is not a manifest.
#[test]
fn nothing_panics_on_damaged_input() {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let dir = fixture_dir().join("manifests");
    let mut seen = 0;
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let body = std::fs::read(entry.path()).unwrap();
        if body.len() > 70_000 {
            continue;
        }
        for _ in 0..150 {
            let mut damaged = body.clone();
            match next() % 5 {
                0 => {
                    let at = (next() as usize) % damaged.len();
                    damaged[at] ^= 1 << (next() % 8);
                }
                1 => damaged.truncate((next() as usize) % damaged.len()),
                2 => {
                    let at = (next() as usize) % damaged.len();
                    damaged.insert(at, (next() & 0xff) as u8);
                }
                3 => {
                    let at = (next() as usize) % damaged.len();
                    damaged.remove(at);
                }
                _ => {
                    let a = (next() as usize) % damaged.len();
                    let b = (next() as usize) % damaged.len();
                    damaged.swap(a, b);
                }
            }
            let _ = verify(&damaged, &keys(), NOW, 0);
            seen += 1;
        }
    }
    assert!(seen > 3000);
    // Payload-level noise through the real signature path.
    for i in 0..300 {
        let junk: String = (0..(next() % 120))
            .map(|_| char::from_u32((next() % 0x2fff) as u32 + 0x20).unwrap_or('x'))
            .collect();
        let _ = run(&junk, 0);
        let _ = run(
            &format!(
                r#"{{"schemaVersion":1,"issuedAt":{i},"expires":{junk:?},"channels":{junk:?}}}"#
            ),
            0,
        );
    }
}

#[test]
fn huge_integers_fail_their_field_not_the_document() {
    let nines = "9".repeat(400);
    // In a field that wants a small integer: the component is dropped.
    let body = sign(&format!(
        r#"{{"schemaVersion":1,"issuedAt":{NOW},"expires":{},"channels":{{"beta":{{"desktop":{{"version":"0.4.0","rollout":{nines}}}}},"stable":{{"desktop":{{"version":"1.0.0"}}}}}}}}"#,
        NOW + 1000
    ));
    let p = verify(&body, &keys(), NOW, 0).unwrap();
    assert!(p.beta.is_none() && p.stable.is_some());
    // A schema too large for 64 bits is a newer schema; a timestamp is a type error.
    let schema = |s: &str| {
        sign(&format!(
            r#"{{"schemaVersion":{s},"issuedAt":1,"expires":2}}"#
        ))
    };
    assert_eq!(
        verify(&schema("9223372036854775808"), &keys(), NOW, 0),
        Err(Reject::Schema)
    );
    assert_eq!(
        verify(&schema(&nines), &keys(), NOW, 0),
        Err(Reject::Schema)
    );
    assert_eq!(verify(&schema("0"), &keys(), NOW, 0), Err(Reject::Parse));
    let issued = sign(&format!(
        r#"{{"schemaVersion":1,"issuedAt":{nines},"expires":2}}"#
    ));
    assert_eq!(verify(&issued, &keys(), NOW, 0), Err(Reject::Parse));
    assert!(strict_json::parse(format!(r#"{{"a":{nines}}}"#).as_bytes()).is_some());
}

#[test]
fn a_positive_link_in_the_link_table() {
    assert!(url_ok(
        "https://github.com/derpx06/DeskLink/releases/tag/v0.4.0",
        LinkField::ReleasePage,
        &v("0.4.0")
    ));
}
