//! A seeded simulation like the Glide ones: random checks, failures, 304s,
//! new manifests, clock jumps, restarts, skips and snoozes against the engine,
//! asserting the invariants after every step.

use super::*;
use crate::update::engine::Engine;
use crate::update::model::Decision;

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

fn manifest(version: &str, min: &str, rollout: u64, issued: u64, now: u64) -> Vec<u8> {
    sign(&format!(
        r#"{{"schemaVersion":1,"issuedAt":{issued},"expires":{},"channels":{{"beta":{{"desktop":{{"version":"{version}","minSupported":"{min}","rollout":{rollout},"notes":[{{"title":"n"}}]}}}}}}}}"#,
        now + 21 * 86_400
    ))
}

fn run(seed: u64) {
    let mut rng = Rng(seed | 1);
    let fake = Fake::default();
    let path = scratch(&format!("sim{seed}"));
    let mut installed = v("0.3.0");
    let mut now = NOW;
    let mut cfg = settings();
    let mut engine = Engine::new(
        Some(fake.clone()),
        keys(),
        installed,
        Some(path.clone()),
        now,
    );
    let mut last_issued = 0u64;
    for step in 0..10_000 {
        let before = (
            engine.store.cached.clone(),
            engine.store.etag.clone(),
            engine.store.last_issued_at,
        );
        let mut failed = false;
        match rng.below(12) {
            0..=2 => {
                let minor = 3 + rng.below(4);
                let min = if rng.below(6) == 0 {
                    format!("0.{}.0", minor)
                } else {
                    "0.0.0".into()
                };
                let issued = last_issued + rng.below(3) * 10;
                fake.body(
                    manifest(
                        &format!("0.{minor}.{}", rng.below(3)),
                        &min,
                        rng.below(101),
                        issued,
                        now,
                    ),
                    &format!("\"{step}\""),
                );
                engine.check(&cfg, now, rng.below(4) == 0);
                last_issued = last_issued.max(issued);
            }
            3 => {
                fake.push(Ok(Fetched::NotModified));
                engine.check(&cfg, now, false);
            }
            4 => {
                let error = [
                    FetchError::Offline,
                    FetchError::Timeout,
                    FetchError::Http,
                    FetchError::Oversized,
                    FetchError::Parse,
                ][rng.below(5) as usize];
                fake.push(Err(error));
                failed = true;
                engine.check(&cfg, now, rng.below(2) == 0);
            }
            5 => {
                // A replayed or forged body.
                let replay = rng.below(2) == 0;
                let body = if replay {
                    manifest("0.9.0", "0.0.0", 100, 1, now)
                } else {
                    let mut b = manifest("0.9.0", "0.0.0", 100, now, now);
                    let k = b.len() - 12;
                    b[k] ^= 1;
                    b
                };
                fake.body(body, "\"forged\"");
                // A "replay" only fails once something newer than 1 was seen.
                failed = !replay || before.2 > 1;
                engine.check(&cfg, now, false);
            }
            6 => now += rng.below(3 * 86_400),
            7 => now += rng.below(10),
            8 => {
                engine = Engine::new(
                    Some(fake.clone()),
                    keys(),
                    installed,
                    Some(path.clone()),
                    now,
                );
            }
            9 => {
                if let Some(c) = engine.decision(&cfg, now, true).component().cloned() {
                    if rng.below(2) == 0 {
                        engine.skip(c.version)
                    } else {
                        engine.snooze(c.version, now)
                    }
                }
            }
            10 => cfg.auto_check = rng.below(3) != 0,
            _ => {
                if rng.below(5) == 0 {
                    installed = Version(installed.0, installed.1 + 1, 0);
                    engine.set_installed(installed);
                }
            }
        }
        // 1. Never offers a version at or below the installed one.
        let auto = engine.decision(&cfg, now, false);
        if let Some(c) = auto.component() {
            if !matches!(auto, Decision::Required(_)) {
                assert!(
                    c.version > installed,
                    "seed {seed} step {step}: offered {} over {installed}",
                    c.version
                );
            }
        }
        // 2. Required lasts until the installed version meets the minimum.
        if let Decision::Required(c) = &auto {
            assert!(installed < c.min_supported);
        }
        // 3. A skipped version is never offered unprompted.
        if let Decision::Available(c) = &auto {
            assert_ne!(
                engine.prefs().skipped,
                Some(c.version),
                "seed {seed} step {step}"
            );
        }
        // 4. The newest verified manifest time only moves forward.
        assert!(
            engine.store.last_issued_at >= before.2,
            "seed {seed} step {step}"
        );
        // 5. A failure persists no manifest state.
        if failed {
            assert_eq!(
                (
                    engine.store.cached.clone(),
                    engine.store.etag.clone(),
                    engine.store.last_issued_at
                ),
                before,
                "seed {seed} step {step}: failed fetch changed state"
            );
        }
    }
}

#[test]
fn ten_thousand_random_steps_keep_every_invariant() {
    for seed in [1, 7, 42, 2026, 99_991] {
        run(seed);
    }
}

#[test]
fn automatic_checks_never_reach_the_network_while_off() {
    let fake = Fake::default();
    let mut e = Engine::new(Some(fake.clone()), keys(), v("0.3.0"), None, NOW);
    let off = Settings {
        auto_check: false,
        ..settings()
    };
    for i in 0..200 {
        e.check(&off, NOW + i * 100_000, false);
    }
    assert_eq!(fake.calls(), 0);
}
