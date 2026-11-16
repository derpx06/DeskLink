//! Property tests without a crate: a fixed-seed generator, so a failure
//! repeats.

use super::*;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn random_entry(rng: &mut Rng) -> Entry {
    let deleted = rng.below(4) == 0;
    Entry {
        size: 1,
        mtime: 1_700_000_000_000,
        sha256: if deleted {
            String::new()
        } else {
            format!("sha{}", rng.below(3))
        },
        deleted,
        dir: false,
        vv: Vv {
            desk: rng.below(4),
            phone: rng.below(4),
        },
    }
}

/// The planner never throws away a local edit: a file removed or
/// replaced here was dominated by the other side's version, or went
/// aside under a conflict name first.
#[test]
fn the_planner_never_loses_a_live_local_edit() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let paths = ["a.txt", "b.txt", "c/d.txt", "e.txt"];
    for case in 0..3000 {
        let mut local = Manifest::new();
        let mut remote = Manifest::new();
        for path in paths {
            if rng.below(4) != 0 {
                local.insert(path.to_string(), random_entry(&mut rng));
            }
            if rng.below(4) != 0 {
                remote.insert(path.to_string(), random_entry(&mut rng));
            }
        }
        for side in [Side::Desk, Side::Phone] {
            let steps = plan(side, &local, &remote, "Pixel");
            for step in &steps {
                let (path, replaces) = match step {
                    Step::Delete { path, .. } | Step::Download { path, .. } => (path, true),
                    _ => continue,
                };
                let (Some(ours), Some(theirs)) = (local.get(path), remote.get(path)) else {
                    continue;
                };
                if ours.deleted || !replaces {
                    continue;
                }
                let kept_aside = steps.iter().any(|other| {
                    matches!(other, Step::Rename { from, .. } if from == path)
                        || matches!(other, Step::Conflict { path: p, .. } if p == path)
                });
                assert!(
                    theirs.vv.dominates(ours.vv) || kept_aside || ours.sha256 == theirs.sha256,
                    "case {case} {side:?}: {path} {ours:?} replaced by {theirs:?}: {steps:?}"
                );
            }
        }
    }
}

/// A path the Bucket accepts has no way out of its folder.
#[test]
fn no_accepted_bucket_path_can_leave_the_folder() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let alphabet: Vec<char> = "ab./\\: \u{0}\u{7}\u{202e}é~".chars().collect();
    for _ in 0..20_000 {
        let length = rng.below(14) as usize;
        let path: String = (0..length)
            .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize])
            .collect();
        for dir in [true, false] {
            if bucket_path_ok(&path, dir) {
                assert!(!path.starts_with('/'), "{path:?}");
                assert!(!path.contains('\\'), "{path:?}");
                assert!(!path
                    .split('/')
                    .any(|s| s.is_empty() || s == "." || s == ".."));
                assert!(!path.chars().any(char::is_control), "{path:?}");
                assert!(path.split('/').count() <= MAX_BUCKET_DEPTH + 1);
            }
        }
    }
}

/// Whatever bytes a peer sends, decoding a packet or a vCard never
/// panics.
#[test]
fn hostile_bytes_never_panic_the_packet_or_vcard_parsers() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let pieces = [
        "{",
        "}",
        "\"",
        ":",
        ",",
        "[",
        "]",
        "null",
        "type",
        "body",
        "desklink.ping",
        "id",
        "-1",
        "BEGIN:VCARD",
        "\r\n",
        "FN:",
        "TEL;TYPE=CELL:",
        "PHOTO;ENCODING=b:",
        "END:VCARD",
        "é",
    ];
    for _ in 0..5_000 {
        let count = rng.below(30);
        let text: String = (0..count)
            .map(|_| pieces[rng.below(pieces.len() as u64) as usize])
            .collect();
        let _ = crate::device_links::packet::NetworkPacket::deserialize(text.as_bytes());
        let _ = crate::device_links::contacts::parse_vcard("u", &text);
    }
    let mut bytes = vec![0u8; 512];
    for _ in 0..2_000 {
        for byte in &mut bytes {
            *byte = rng.next() as u8;
        }
        let _ = crate::device_links::packet::NetworkPacket::deserialize(&bytes);
    }
}
