//! Verification, policy, channels, rollout and the decision: the shared
//! table, then the cases it does not hold.

use super::*;
use crate::update::model::*;

fn prefs_of(run: &serde_json::Value, defaults: &serde_json::Value) -> Prefs {
    let p = run.get("prefs").unwrap_or(&defaults["prefs"]);
    Prefs {
        skipped: p["skipped"].as_str().map(v),
        snoozed: p["snoozedVersion"].as_str().map(v),
        snoozed_until: p["snoozedUntil"].as_u64().unwrap_or(0),
    }
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).unwrap())
        .collect()
}

fn verdict(decision: &Decision) -> String {
    match decision {
        Decision::UpToDate => "UpToDate".into(),
        Decision::Available(c) => format!("Offer:{}", c.version),
        Decision::Required(c) => format!("Required:{}", c.version),
        Decision::Skipped(c) => format!("Skipped:{}", c.version),
        Decision::Snoozed(c, _) => format!("Snoozed:{}", c.version),
    }
}

fn glyph_name(glyph: Glyph) -> &'static str {
    match glyph {
        Glyph::Refresh => "refresh",
        Glyph::Swap => "swap",
        Glyph::Check => "check",
        Glyph::Files => "files",
        Glyph::Terminal => "terminal",
        Glyph::Speed => "speed",
        Glyph::Shield => "shield",
        Glyph::Star => "star",
    }
}

/// Every manifest and run of the shared table, desktop column.
#[test]
fn the_shared_table_gives_every_verdict() {
    let cases = json("cases.json");
    let defaults = &cases["defaults"];
    let all_keys = keys();
    let mut runs = 0;
    for case in cases["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let body = fixture(case["file"].as_str().unwrap());
        for run in case["runs"].as_array().unwrap() {
            let name = run["name"].as_str().unwrap();
            let get = |field: &str| run.get(field).unwrap_or(&defaults[field]).clone();
            let now = get("now").as_u64().unwrap();
            let last_seen = get("lastSeenIssuedAt").as_u64().unwrap();
            // An installed version that is not strict semver (a build bug)
            // offers nothing and requires nothing.
            let Some(installed) =
                Version::parse(get("installed")["desktop"].as_str().unwrap_or(""))
            else {
                let want = match &run["expect"] {
                    serde_json::Value::String(text) => text.clone(),
                    other => other["desktop"].as_str().unwrap().to_string(),
                };
                if !want.starts_with("Ignore:") {
                    assert_eq!(want, "UpToDate", "{id} / {name}");
                }
                runs += 1;
                continue;
            };
            let channel = Channel::chosen(get("channel").as_str().unwrap(), installed);
            let manual = get("manual").as_bool().unwrap();
            let id_bytes = hex(get("installId").as_str().unwrap());
            let set: Vec<Key> = get("keys")
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|kid| {
                    all_keys
                        .iter()
                        .filter(|k| k.kid == kid.as_str().unwrap())
                        .cloned()
                })
                .collect();
            let result = verify(&body, &set, now, last_seen);
            let expect = &run["expect"];
            let got = match &result {
                Err(reject) => format!("Ignore:{}", reject.code()),
                Ok(payload) => verdict(&decide(
                    installed,
                    channel,
                    payload,
                    &prefs_of(run, defaults),
                    &id_bytes,
                    now,
                    manual,
                )),
            };
            let want = match expect {
                serde_json::Value::String(text) => text.clone(),
                other => other["desktop"].as_str().unwrap().to_string(),
            };
            assert_eq!(got, want, "{id} / {name}");
            runs += 1;
            let Ok(payload) = result else { continue };
            let component = select(&payload, channel);
            if let Some(want) = run.get("expectNotes") {
                let component = component.unwrap_or_else(|| panic!("{id}/{name}: no component"));
                let got: Vec<serde_json::Value> = component
                    .notes
                    .iter()
                    .map(|n| serde_json::json!({"title": n.title, "detail": n.detail, "glyph": glyph_name(n.glyph)}))
                    .collect();
                assert_eq!(&serde_json::Value::Array(got), want, "{id}/{name}: notes");
            }
            if let Some(want) = run.get("expectRequiredReason") {
                let got = component.and_then(|c| c.required_reason.clone());
                assert_eq!(serde_json::json!(got), *want, "{id}/{name}: requiredReason");
            }
            if let Some(want) = run.get("expectReleased") {
                let got = component.and_then(|c| c.released.clone());
                assert_eq!(serde_json::json!(got), *want, "{id}/{name}: released");
            }
            if let Some(urls) = run.get("expectUrls") {
                let component = component.unwrap_or_else(|| panic!("{id}/{name}: no component"));
                for (field, state) in urls.as_object().unwrap() {
                    let kept = match field.as_str() {
                        "desktop.tarball" => component.tarball.is_some(),
                        "desktop.notesUrl" => component.notes_url.is_some(),
                        "desktop.releaseUrl" => component.release_url.is_some(),
                        _ => continue,
                    };
                    assert_eq!(kept, state == "kept", "{id}/{name}: {field}");
                }
            }
        }
    }
    assert!(runs > 150, "the table ran {runs} runs");
}

#[test]
fn the_ratchet_never_runs_ahead_of_the_clock() {
    for vector in json("cases.json")["ratchetVectors"].as_array().unwrap() {
        assert_eq!(
            ratchet(
                vector["issuedAt"].as_u64().unwrap(),
                vector["now"].as_u64().unwrap()
            ),
            vector["stored"].as_u64().unwrap(),
            "{vector}"
        );
    }
}

#[test]
fn the_bucket_vectors_agree_with_the_other_apps() {
    for vector in json("cases.json")["bucketVectors"].as_array().unwrap() {
        let got = bucket(
            &hex(vector["installId"].as_str().unwrap()),
            vector["component"].as_str().unwrap(),
            &v(vector["version"].as_str().unwrap()),
        );
        assert_eq!(got as u64, vector["bucket"].as_u64().unwrap(), "{vector}");
    }
}

/// The shared URL table, for the desktop shapes: the release page and the
/// tarball of the DeskLink repo, for the version in the link.
#[test]
fn the_link_table() {
    for check in json("cases.json")["urlChecks"].as_array().unwrap() {
        let url = check[0].as_str().unwrap();
        let allowed = check[1].as_bool().unwrap();
        let got = ["0.4.0", "0.3.1", "1.0.0", "1.2.3"].iter().any(|ver| {
            [LinkField::ReleasePage, LinkField::Tarball]
                .iter()
                .any(|field| url_ok(url, *field, &v(ver)))
        });
        // A desktop link is allowed only where the table allows the link at
        // all; the android and Play shapes are refused here and that is fine.
        if !allowed {
            assert!(!got, "{url}");
        }
    }
}

/// The same shapes by component, field and version: the desktop rows must
/// agree; the android shapes must never pass as a desktop link.
#[test]
fn the_context_link_table() {
    let mut checked = 0;
    for row in json("cases.json")["urlContextChecks"].as_array().unwrap() {
        let (url, component, field, version, allowed) = (
            row[0].as_str().unwrap(),
            row[1].as_str(),
            row[2].as_str(),
            row[3].as_str(),
            row[4].as_bool().unwrap(),
        );
        let (Some(component), Some(field), Some(version)) = (component, field, version) else {
            continue;
        };
        let field = match field {
            "notesUrl" | "releaseUrl" => LinkField::ReleasePage,
            "tarball" => LinkField::Tarball,
            _ => continue,
        };
        if component == "desktop" {
            assert_eq!(url_ok(url, field, &v(version)), allowed, "{row}");
            checked += 1;
        } else if allowed {
            assert!(
                !url_ok(url, field, &v(version)),
                "{row}: an android link is not a desktop link"
            );
        }
    }
    assert!(checked >= 6);
}

#[test]
fn install_ids_must_be_lower_case_hex_of_16_bytes() {
    for row in json("cases.json")["installIdChecks"].as_array().unwrap() {
        assert_eq!(
            crate::update::store::install_id_ok(row[0].as_str().unwrap()),
            row[1].as_bool().unwrap(),
            "{row}"
        );
    }
}

#[test]
fn the_shared_signature_vector_verifies() {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B;
    let vector = json("vector.json");
    let key = Key {
        kid: vector["kid"].as_str().unwrap().to_string(),
        public: B
            .decode(vector["publicKeyBase64url"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap(),
    };
    let message = B
        .decode(vector["payloadBase64url"].as_str().unwrap())
        .unwrap();
    let sig = B
        .decode(vector["signatureBase64url"].as_str().unwrap())
        .unwrap();
    assert!(super::super::model::verify(b"{}", std::slice::from_ref(&key), NOW, 0).is_err());
    // The body is an envelope around that payload: sign nothing here, only
    // check the primitive through a hand-made envelope with the real sig.
    let envelope = serde_json::json!({"v":1,"kid":vector["kid"],"payload":B.encode(&message),"sig":B.encode(&sig)});
    // The payload is not JSON, so the signature passes and the parse fails.
    assert_eq!(
        super::super::model::verify(envelope.to_string().as_bytes(), &[key], NOW, 0),
        Err(Reject::Parse)
    );
}

#[test]
fn versions_parse_strictly_and_order_as_numbers() {
    assert!(v("0.3.0") < v("0.4.0") && v("0.4.0") < v("1.0.0") && v("0.9.0") < v("0.10.0"));
    for bad in [
        "",
        "1",
        "1.2",
        "1.2.3.4",
        "01.2.3",
        "1.2.x",
        "1.2.3-beta",
        "v1.2.3",
        " 1.2.3",
        "1..3",
        "+1.2.3",
        "99999999999.0.0",
        "2147483648.0.0",
        "1.2.3+b",
    ] {
        assert_eq!(Version::parse(bad), None, "{bad:?}");
    }
    assert_eq!(v("0.3.0").to_string(), "0.3.0");
}

#[test]
fn the_channel_default_follows_the_line() {
    assert_eq!(Channel::chosen("", v("0.3.0")), Channel::Beta);
    assert_eq!(Channel::chosen("", v("1.0.0")), Channel::Stable);
    assert_eq!(Channel::chosen("stable", v("0.3.0")), Channel::Stable);
    assert_eq!(Channel::chosen("beta", v("1.2.0")), Channel::Beta);
    assert_eq!(Channel::chosen("nonsense", v("1.2.0")), Channel::Stable);
}

#[test]
fn manifest_text_is_accepted_or_refused_whole() {
    assert!(text_ok("Files has a list view", 40));
    assert!(
        text_ok(&"é".repeat(40), 40) && !text_ok(&"é".repeat(41), 40),
        "scalars, not bytes"
    );
    assert!(text_ok(&"\u{1f680}".repeat(40), 40) && !text_ok(&"\u{1f680}".repeat(41), 40));
    for bad in [
        "",
        "a\nb",
        "a\u{0}b",
        "a\tb",
        "x\u{202e}y",
        "x\u{2066}y",
        "a\u{200b}",
        "\u{feff}a",
        "a\u{2028}b",
        "a\u{2029}",
        "<b>x</b>",
        "a & b",
        "a > b",
        "a\u{7f}",
    ] {
        assert!(!text_ok(bad, 80), "{bad:?}");
    }
    assert!(
        text_ok("**not bold** `code` [x](y) # n", 80),
        "markdown characters are plain text"
    );
}

#[test]
fn a_component_that_is_not_usable_offers_nothing() {
    let sign = |p: &str| super::sign(p);
    let make = |component: &str| {
        sign(&format!(
            r#"{{"schemaVersion":1,"issuedAt":{NOW},"expires":{},"channels":{{"beta":{{"desktop":{component}}}}}}}"#,
            NOW + 1000
        ))
    };
    let offered = |component: &str| verify(&make(component), &keys(), NOW, 0).unwrap().beta;
    assert!(offered(r#"{"version":"0.4.0","notes":[{"title":"a"}]}"#).is_some());
    for bad in [
        r#"{"version":"0.4"}"#,
        r#"{"version":"0.4.0","rollout":101}"#,
        r#"{"version":"0.4.0","minSupported":"x"}"#,
        r#"{"version":"0.4.0","minSupported":"0.5.0"}"#,
        r#"{"released":"2026-12-08"}"#,
        "null",
    ] {
        assert!(offered(bad).is_none(), "{bad}");
    }
    // A sign, fraction or exponent spoils the whole document, not one field.
    for bad in ["-1", "50.5", "5e1", "1e999", "01"] {
        let body = make(&format!(r#"{{"version":"0.4.0","rollout":{bad}}}"#));
        assert_eq!(verify(&body, &keys(), NOW, 0), Err(Reject::Parse), "{bad}");
    }
    // Absent rollout means everyone; a note without a title is skipped.
    let c = offered(r#"{"version":"0.4.0","notes":[{"detail":"x"},{"title":"ok"}]}"#).unwrap();
    assert_eq!((c.rollout, c.notes.len()), (100, 1));
    // A bad note is skipped whole, then the first 8 of the rest are kept.
    let many: Vec<String> = (0..12)
        .map(|i| {
            if i % 3 == 0 {
                r#"{"title":"<bad>"}"#.to_string()
            } else {
                format!(r#"{{"title":"n{i}"}}"#)
            }
        })
        .collect();
    let c = offered(&format!(
        r#"{{"version":"0.4.0","notes":[{}]}}"#,
        many.join(",")
    ))
    .unwrap();
    assert_eq!(c.notes.len(), 8);
    assert!(c.notes.iter().all(|n| !n.title.contains('<')));
    // The date must be real.
    let date = |d: &str| {
        offered(&format!(r#"{{"version":"0.4.0","released":"{d}"}}"#))
            .unwrap()
            .released
    };
    assert_eq!(date("2026-12-08").as_deref(), Some("2026-12-08"));
    assert_eq!(date("2024-02-29").as_deref(), Some("2024-02-29"));
    for bad in [
        "2026-02-29",
        "2026-13-01",
        "2026-00-10",
        "2026-1-1",
        "26-12-08",
        "2026/12/08",
        "2026-12-08T00",
    ] {
        assert_eq!(date(bad), None, "{bad}");
    }
}

#[test]
fn the_rollout_edges() {
    let id = [1u8; 16];
    let component = |rollout| Component {
        version: v("0.4.0"),
        released: None,
        min_supported: v("0.0.0"),
        required_reason: None,
        rollout,
        notes: vec![],
        notes_url: None,
        release_url: None,
        tarball: None,
    };
    let b = bucket(&id, "desktop", &v("0.4.0")) as u8;
    let payload = |rollout| Payload {
        issued_at: 1,
        expires: u64::MAX,
        beta: Some(component(rollout)),
        stable: None,
    };
    let run = |rollout| {
        decide(
            v("0.3.0"),
            Channel::Beta,
            &payload(rollout),
            &Prefs::default(),
            &id,
            NOW,
            false,
        )
    };
    assert_eq!(run(0), Decision::UpToDate);
    assert_eq!(run(100), Decision::Available(component(100)));
    assert_eq!(
        run(b),
        Decision::UpToDate,
        "a bucket equal to the rollout is not offered"
    );
    assert_eq!(run(b + 1), Decision::Available(component(b + 1)));
}
