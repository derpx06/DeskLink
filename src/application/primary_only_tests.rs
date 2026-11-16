//! A second launch does not start the daemon (pinned on mod.rs's imp).

/// I14: `constructed` runs in a second launch too, so it must not start
/// the daemon (port 1717, beacons, relay client) or the terminal server.
#[test]
fn a_second_launch_does_not_start_the_daemon() {
    let source = include_str!("mod.rs");
    let start = source.find("fn constructed(&self)").unwrap();
    let end = start
        + source[start..]
            .find("fn startup")
            .map_or(600, |i| i.min(600));
    let constructed = &source[start..end];
    assert!(
        !constructed.contains("DaemonHandle::start"),
        "{constructed}"
    );
    assert!(!constructed.contains("start_server"));
    assert!(source.contains("fn start_primary"));
    // And the only caller of `start_primary` is `startup`, which GApplication
    // runs in the primary instance only.
    let startup = source
        .find("fn startup(&self) {\n            self.parent_startup();")
        .unwrap();
    let calls: Vec<usize> = source
        .match_indices(concat!("self.start_primary", "()"))
        .map(|(at, _)| at)
        .collect();
    assert_eq!(calls.len(), 1, "one call");
    assert!(
        calls[0] > startup && calls[0] < startup + 200,
        "made from `startup`"
    );
}
