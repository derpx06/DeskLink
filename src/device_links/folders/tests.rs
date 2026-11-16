//! Tests of folder sync and the Bucket: the worker driven through its
//! messages and packets, scans on scratch folders, the planner, imports,
//! the wire limits and case clashes. Each test runs on its own thread-local
//! home, trash and report queue.

use super::*;

#[test]
fn a_full_bucket_manifest_fits_the_wire_part_by_part() {
    let mut manifest = Manifest::new();
    for index in 0..MAX_BUCKET_ITEMS {
        manifest.insert(
            format!("{index:05} {}.jpg", "\u{10348}".repeat(40)),
            entry(&"a".repeat(64), (u64::MAX, u64::MAX)),
        );
    }
    let packets = manifest_packets(BUCKET_ID, &manifest);
    assert!(packets.len() <= MAX_PARTS, "{} parts", packets.len());
    for packet in &packets {
        assert!(
            crate::device_links::webrtc::fits_on_wire(packet),
            "{:?}",
            crate::device_links::webrtc::enveloped_len(packet)
        );
    }
}

/// C4: a gone folder reads "The folder is missing", and the fix button's
/// action makes it again (empty, which the engine then holds).
#[test]
fn a_missing_bucket_folder_can_be_created_again() {
    let base = std::env::temp_dir().join(format!("desklink-missing-{}", uuid::Uuid::new_v4()));
    let root = base.join("Bucket");
    let pair = Pair {
        id: BUCKET_ID.into(),
        name: "DeskLink Bucket".into(),
        root: root.clone(),
        device: "phone-b".into(),
        include_hidden: false,
        ignore: Vec::new(),
        bucket: true,
    };
    assert_eq!(scan(&pair).err().as_deref(), Some(FOLDER_MISSING));
    recreate_root(&root).unwrap();
    assert!(scan(&pair).is_ok());
    let _ = fs::remove_dir_all(&base);
}

fn bucket_worker(base: &Path, root: &Path) -> Worker {
    let mut worker = bucket_worker_for(base, root, "phone-b");
    worker.ready = ["phone-b".to_string()].into();
    worker
}

/// A worker with `device`'s Bucket, the phone not yet ready.
fn bucket_worker_for(base: &Path, root: &Path, device: &str) -> Worker {
    let pair = Pair {
        id: BUCKET_ID.into(),
        name: "DeskLink Bucket".into(),
        root: root.to_path_buf(),
        device: device.into(),
        include_hidden: false,
        ignore: Vec::new(),
        bucket: true,
    };
    let worker = Worker {
        dir: base.join("state"),
        pairs: vec![Live::new(pair, Book::default())],
        ready: HashSet::new(),
        asked: HashMap::new(),
        offered: Vec::new(),
        dirty: Default::default(),
        dropped: 0,
    };
    fs::create_dir_all(&worker.dir).unwrap();
    worker
}

fn scratch(name: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("desklink-{name}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&base).unwrap();
    base
}

// ── Folder sync starts when the phone is feature-ready ────────────────

/// Regression: the W1 removals deleted `folders::link(id, true)` together
/// with the automations call beside it, so the worker never counted a phone
/// as ready and the Bucket never synced. The chain: a link goes
/// feature-ready, the folder worker is told, a file written into the
/// Bucket's folder is scanned and its manifest goes out.
#[test]
fn a_feature_ready_link_makes_the_bucket_sync_and_send_its_manifest() {
    let device = format!(
        "{}{}",
        super::super::desk_features::TEST_OUTBOX_PREFIX,
        uuid::Uuid::new_v4().simple()
    );
    let base = scratch("ready-chain");
    let root = base.join("bucket");
    fs::create_dir_all(&root).unwrap();
    let mut worker = bucket_worker_for(&base, &root, &device);
    fs::write(root.join("hello.txt"), "hi").unwrap();
    let manifests = |device: &str| {
        super::super::desk_features::test_sent(device)
            .into_iter()
            .filter(|packet| {
                packet.packet_type == PACKET_TYPE_FOLDERS
                    && packet.get_str("op") == Some("manifest")
            })
            .collect::<Vec<_>>()
    };
    // A phone that is not ready is not synced, however long it waits.
    for _ in 0..3 {
        worker.tick();
    }
    assert!(manifests(&device).is_empty());

    // The link goes feature-ready: this is what the daemon calls.
    super::super::desk_features::on_feature_ready(&device);
    let linked = |device: &str| -> Vec<bool> {
        TEST_LINKS
            .lock()
            .unwrap()
            .iter()
            .filter(|(owner, _)| owner == device)
            .map(|(_, ready)| *ready)
            .collect()
    };
    let waited = Instant::now();
    while linked(&device).is_empty() {
        assert!(
            waited.elapsed() < Duration::from_secs(10),
            "feature-ready never told the folder worker"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(linked(&device), [true]);

    // The folder worker hears it, scans, and sends the manifest.
    worker.handle(Message::Link(device.clone(), true));
    worker.tick();
    let sent = manifests(&device);
    assert_eq!(sent.len(), 1, "one manifest");
    let listed = serde_json::Value::Object(sent[0].body.clone()).to_string();
    assert!(listed.contains("hello.txt"), "{listed}");
    let pairs = super::super::desk_features::test_sent(&device)
        .into_iter()
        .any(|packet| packet.get_str("op") == Some("pairs"));
    assert!(pairs, "the phone is told which pairs this side has");
    let _ = fs::remove_dir_all(base);
}

// ── F8c ────────────────────────────────────────────────────────────────

fn scratch_bucket(name: &str) -> (PathBuf, PathBuf, Worker) {
    let base = scratch(name);
    let root = base.join("bucket");
    fs::create_dir_all(&root).unwrap();
    let worker = bucket_worker(&base, &root);
    (base, root, worker)
}

fn trashed() -> usize {
    TEST_TRASHED.with(std::cell::Cell::get)
}

#[test]
fn the_limits_are_the_documented_ones() {
    assert_eq!(MAX_FILE_BYTES, 1 << 30, "1 GiB per file");
    assert_eq!(MAX_BUCKET_DEPTH, 2);
    assert_eq!(MAX_IN_FLIGHT, 1, "the phone mirrors it");
    assert_eq!((MAX_ADD_ITEMS, MAX_ADD_BYTES), (500, 5 << 30));
    assert_eq!(BUCKET_WARN_BYTES, 8 << 30, "warning at 8 GiB");
    assert_eq!(BUCKET_FULL_BYTES, 10 << 30, "refusal at 10 GiB");
    assert_eq!(MAX_BUCKET_ITEMS, 10_000);
    assert_eq!(MAX_NAME_BYTES, 255);
    assert_eq!(MAX_TEXT_ITEM_BYTES, 1 << 20);
    assert_eq!(STABLE_FOR, Duration::from_secs(2));
    assert_eq!(LANE_QUEUE, 8, "copy jobs waiting before \"busy\"");
}

#[test]
fn names_that_another_system_cannot_hold_are_not_synced() {
    for good in ["a.txt", "Trip 2026", "naïve – café.md", &"x".repeat(255)] {
        assert!(name_ok(good), "{good}");
    }
    for bad in [
        "",
        ".",
        "..",
        "a:b",
        "a\\b",
        "trailing.",
        "trailing ",
        "a\u{7}b",
        "invoice\u{202E}fdp.exe",
        "a\u{2066}b",
        &"x".repeat(256),
        &"é".repeat(128),
    ] {
        assert!(!name_ok(bad), "{bad:?}");
    }
    assert!(bucket_path_ok("A/B", true) && bucket_path_ok("A/B/c.txt", false));
    assert!(!bucket_path_ok("A/B/C", true) && !bucket_path_ok("A/B/C/d.txt", false));
    // Scanned from disk: listed with why, never synced.
    let (base, root, worker) = scratch_bucket("bucket-names");
    fs::write(root.join("ok.txt"), "1").unwrap();
    fs::write(root.join("a:b.txt"), "1").unwrap();
    fs::write(root.join("dot."), "1").unwrap();
    // Not text (Linux allows it): listed too, not skipped in silence.
    {
        use std::os::unix::ffi::OsStrExt;
        fs::write(
            root.join(std::ffi::OsStr::from_bytes(b"bad\xffname.txt")),
            "1",
        )
        .unwrap();
    }
    let (found, _, _, skipped) = scan(&worker.pairs[0].pair).unwrap();
    assert_eq!(found.keys().collect::<Vec<_>>(), ["ok.txt"]);
    assert_eq!(skipped.len(), 3, "{skipped:?}");
    assert!(skipped
        .iter()
        .all(|line| line.contains("the name can't sync")));
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn depth_three_is_refused_and_reported_everywhere() {
    let (base, root, worker) = scratch_bucket("bucket-depth");
    fs::create_dir_all(root.join("A/B/C/D")).unwrap();
    fs::write(root.join("A/B/ok.txt"), "x").unwrap();
    fs::write(root.join("A/B/C/deep.txt"), "x").unwrap();
    std::os::unix::fs::symlink(base.join("elsewhere"), root.join("link")).unwrap();
    // From outside, in the Bucket folder: listed as not synced.
    let (found, _, _, skipped) = scan(&worker.pairs[0].pair).unwrap();
    assert_eq!(
        found.keys().map(String::as_str).collect::<Vec<_>>(),
        ["A", "A/B", "A/B/ok.txt"]
    );
    assert!(found["A"].dir && found["A/B"].dir && !found["A/B/ok.txt"].dir);
    assert!(skipped
        .iter()
        .any(|line| line == "A/B/C (more than 2 folders deep)"));
    assert!(
        skipped.iter().any(|line| line.starts_with("link (")),
        "links listed"
    );
    // From the phone: a deeper path is dropped, a dir entry needs no hash.
    let mut packet = NetworkPacket::new(PACKET_TYPE_FOLDERS);
    let file = |path: &str| {
        json!({"path": path, "size": 1, "mtime": 1, "sha256": "a".repeat(64),
               "vv": {"desk": 0, "phone": 1}})
    };
    let folder = |path: &str| json!({"path": path, "dir": true, "vv": {"desk": 0, "phone": 1}});
    packet.set(
        "entries",
        json!([
            file("A/B/C/d.txt"),
            file("A/B/e.txt"),
            folder("A/B/C"),
            folder("A/B")
        ]),
    );
    let kept: Vec<String> = manifest_entries(&packet, &worker.pairs[0].pair)
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert_eq!(kept, ["A/B/e.txt", "A/B"]);
    let wanted = FolderRef {
        folder: BUCKET_ID.into(),
        relpath: "A/B/C/d.txt".into(),
        mtime: None,
    };
    test_want("phone-b", &wanted, &"a".repeat(64), 1);
    assert!(
        !accepts("phone-b", &wanted, &"a".repeat(64), 1),
        "never accepted"
    );
    // From a drop: counted, and said.
    let outside = base.join("outside");
    fs::create_dir_all(outside.join("T/U/V")).unwrap();
    fs::write(outside.join("T/U/V/w.txt"), "x").unwrap();
    fs::write(outside.join("T/U/x.txt"), "x").unwrap();
    let mut report = Report::default();
    let adds = collect(&[outside.join("T")], &root, "A", (0, 0), &mut report);
    // Into A, T is level 2 and U would be level 3.
    assert_eq!(
        adds.iter().map(|add| add.rel.as_str()).collect::<Vec<_>>(),
        ["A/T"]
    );
    assert_eq!(report.too_deep, 4, "U, V and the two files in them");
    assert_eq!(
        report.message().unwrap(),
        "Some items were more than 2 folders deep and were left out (4)"
    );
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_nested_round_trip_with_an_empty_folder_converges() {
    let mut desk = Replica {
        side: Side::Desk,
        disk: Disk::new(),
        manifest: Manifest::new(),
    };
    let mut phone = Replica {
        side: Side::Phone,
        disk: Disk::new(),
        manifest: Manifest::new(),
    };
    for dir in ["A", "A/B", "Empty"] {
        desk.disk.insert(dir.into(), DIR.into());
    }
    desk.disk.insert("A/B/deep.txt".into(), "deep".into());
    desk.disk.insert("top.txt".into(), "top".into());
    desk.scan(1);
    sync(&mut phone, &desk);
    phone.scan(2);
    assert_eq!(
        desk.disk, phone.disk,
        "folders, the empty one too, and files"
    );
    assert!(phone.manifest["Empty"].dir && phone.manifest["A/B"].dir);
    // And back: the phone adds a folder and a file inside the desk's.
    phone.disk.insert("A/New".into(), DIR.into());
    phone.disk.insert("A/New/p.txt".into(), "phone".into());
    phone.scan(3);
    sync(&mut desk, &phone);
    desk.scan(4);
    assert_eq!(desk.disk, phone.disk);
    // The empty folder is removed on one side: the other follows.
    desk.disk.remove("Empty");
    desk.scan(5);
    sync(&mut phone, &desk);
    phone.scan(6);
    assert_eq!(desk.disk, phone.disk);
    assert!(!phone.disk.contains_key("Empty"));
}

#[test]
fn a_folder_rename_moves_its_files_and_transfers_nothing() {
    let mut desk = Replica {
        side: Side::Desk,
        disk: Disk::new(),
        manifest: Manifest::new(),
    };
    let mut phone = Replica {
        side: Side::Phone,
        disk: Disk::new(),
        manifest: Manifest::new(),
    };
    desk.disk.insert("Trips".into(), DIR.into());
    desk.disk.insert("Trips/Rome".into(), DIR.into());
    desk.disk.insert("Trips/Rome/a.jpg".into(), "aaa".into());
    desk.disk.insert("Trips/b.jpg".into(), "bbb".into());
    desk.scan(1);
    sync(&mut phone, &desk);
    phone.scan(2);
    // The desk renames the folder.
    let moved: Vec<(String, String)> = desk
        .disk
        .iter()
        .map(|(path, content)| (path.replacen("Trips", "Journeys", 1), content.clone()))
        .collect();
    desk.disk = moved.into_iter().collect();
    desk.scan(3);
    let steps = plan(Side::Phone, &phone.manifest, &desk.manifest, "P");
    let downloads: Vec<&Step> = steps
        .iter()
        .filter(|step| matches!(step, Step::Download { entry, .. } if !entry.dir))
        .collect();
    assert!(downloads.is_empty(), "nothing re-sent: {downloads:?}");
    assert_eq!(
        steps
            .iter()
            .filter(|step| matches!(step, Step::Move { .. }))
            .count(),
        2,
        "both files moved by hash"
    );
    sync(&mut phone, &desk);
    phone.scan(4);
    assert_eq!(desk.disk, phone.disk);
    assert!(phone.disk.contains_key("Journeys/Rome/a.jpg"));
    assert!(
        !phone.disk.contains_key("Trips"),
        "the old folder went, empty"
    );
}

#[test]
fn a_rename_and_an_edit_on_opposite_sides_lose_nothing_and_converge() {
    let mut desk = Replica {
        side: Side::Desk,
        disk: Disk::new(),
        manifest: Manifest::new(),
    };
    let mut phone = Replica {
        side: Side::Phone,
        disk: Disk::new(),
        manifest: Manifest::new(),
    };
    desk.disk.insert("A".into(), DIR.into());
    desk.disk.insert("A/f.txt".into(), "base".into());
    desk.scan(1);
    sync(&mut phone, &desk);
    phone.scan(2);
    // The desk renames the folder; the phone edits a file inside it.
    desk.disk.remove("A");
    desk.disk.remove("A/f.txt");
    desk.disk.insert("B".into(), DIR.into());
    desk.disk.insert("B/f.txt".into(), "base".into());
    phone.disk.insert("A/f.txt".into(), "phone edit".into());
    desk.scan(3);
    phone.scan(3);
    for round in 4..9 {
        sync(&mut desk, &phone);
        desk.scan(round);
        sync(&mut phone, &desk);
        phone.scan(round);
    }
    assert_eq!(desk.disk, phone.disk, "converged");
    assert!(
        desk.disk.values().any(|content| content == "phone edit"),
        "the edit is not lost: {:?}",
        desk.disk
    );
    assert!(desk.disk.contains_key("B/f.txt"), "the rename is applied");
}

/// S2-5: "on <phone> too" only when the phone's own manifest lists the
/// file; online with the phone not having it yet is waiting.
#[test]
fn a_file_the_phone_does_not_have_is_not_shown_as_synced() {
    let (base, root, mut worker) = scratch_bucket("bucket-not-there");
    fs::write(root.join("a.txt"), "a").unwrap();
    worker.rescan(0);
    let state = |worker: &Worker| {
        worker.pairs[0]
            .view(true)
            .items
            .iter()
            .find(|item| item.path == "a.txt")
            .map(|item| item.state)
    };
    assert_eq!(state(&worker), Some(ItemState::Waiting));
    let sha = worker.pairs[0].book.manifest["a.txt"].sha256.clone();
    worker.pairs[0].remote.insert("a.txt".into(), sha);
    assert_eq!(state(&worker), Some(ItemState::Synced));
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn removing_everything_reaches_the_phone_and_a_later_empty_scan_is_still_held() {
    let (base, root, mut worker) = scratch_bucket("bucket-remove-all");
    fs::create_dir_all(root.join("Folder")).unwrap();
    fs::write(root.join("Folder/in.txt"), "i").unwrap();
    fs::write(root.join("a.txt"), "a").unwrap();
    worker.rescan(0);
    // Select All → Remove from Both.
    let all: Vec<String> = worker.pairs[0]
        .view(true)
        .items
        .iter()
        .filter(|item| !item.path.contains('/'))
        .map(|item| item.path.clone())
        .collect();
    assert_eq!(all.len(), 2);
    worker.handle(Message::RemoveItems {
        device: "phone-b".into(),
        paths: all,
    });
    assert!(
        worker.pairs[0].allow_empty,
        "a deliberate removal that empties the root"
    );
    worker.rescan(0);
    assert_ne!(worker.pairs[0].phase, Phase::EmptyHeld);
    assert!(worker.pairs[0]
        .book
        .manifest
        .values()
        .all(|entry| entry.deleted));
    assert!(!worker.pairs[0].allow_empty, "spent by that scan");
    // The same flag is spent even when the scan was not empty, so an
    // unmounted drive later is held.
    fs::write(root.join("b.txt"), "b").unwrap();
    worker.pairs[0].allow_empty = true;
    worker.rescan(0);
    assert!(!worker.pairs[0].allow_empty);
    // An unmounted drive: the folder is another directory now, empty.
    fs::remove_dir_all(&root).unwrap();
    fs::create_dir_all(&root).unwrap();
    worker.rescan(0);
    assert_eq!(worker.pairs[0].phase, Phase::EmptyHeld);
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn removing_a_folder_whose_files_are_not_fetched_yet_keeps_them_from_coming_back() {
    let (base, root, mut worker) = scratch_bucket("bucket-remove-unfetched");
    fs::create_dir_all(root.join("A")).unwrap();
    let sha_a = "a".repeat(64);
    let packet = folders_packet(
        "manifest",
        json!({"id": BUCKET_ID, "part": 1, "parts": 1, "entries": [
            {"path": "A", "dir": true, "vv": {"desk": 0, "phone": 1}},
            {"path": "A/x.txt", "size": 5, "mtime": 1, "sha256": sha_a,
             "vv": {"desk": 0, "phone": 3}},
            {"path": "y.txt", "size": 5, "mtime": 1, "sha256": sha_a,
             "vv": {"desk": 0, "phone": 2}},
        ]}),
    );
    worker.on_packet("phone-b", &packet);
    assert_eq!(
        worker.pairs[0].receiving.len(),
        2,
        "both asked for, not here"
    );
    let wanted = |path: &str| FolderRef {
        folder: BUCKET_ID.into(),
        relpath: path.into(),
        mtime: None,
    };
    assert!(accepts("phone-b", &wanted("A/x.txt"), &sha_a, 5));
    worker.handle(Message::RemoveItems {
        device: "phone-b".into(),
        paths: vec!["A".into(), "y.txt".into()],
    });
    let live = &worker.pairs[0];
    assert!(live.receiving.is_empty() && live.outstanding.is_empty());
    assert!(
        !accepts("phone-b", &wanted("A/x.txt"), &sha_a, 5),
        "no longer wanted"
    );
    let tomb = &live.book.manifest["A/x.txt"];
    assert!(
        tomb.deleted && tomb.vv == Vv { desk: 1, phone: 3 },
        "{tomb:?}"
    );
    assert!(live.book.manifest["y.txt"].deleted);
    assert!(live
        .view(true)
        .items
        .iter()
        .all(|item| item.state != ItemState::Receiving));
    // The phone's next manifest still lists them: ours dominates.
    let steps = plan(
        Side::Desk,
        &live.book.manifest,
        &{
            let mut theirs = Manifest::new();
            theirs.insert(
                "A/x.txt".into(),
                Entry {
                    vv: Vv { desk: 0, phone: 3 },
                    ..entry(&sha_a, (0, 3))
                },
            );
            theirs
        },
        "P",
    );
    assert!(steps.is_empty(), "nothing comes back: {steps:?}");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_future_mtime_is_not_held_for_ever() {
    let now = 10_000_000i64;
    let mut found: BTreeMap<String, Stat> = [
        (
            "future.txt".to_string(),
            Stat {
                size: 1,
                mtime: now + 3_600_000,
                dir: false,
            },
        ),
        (
            "fresh.txt".to_string(),
            Stat {
                size: 1,
                mtime: now - 100,
                dir: false,
            },
        ),
        (
            "exact.txt".to_string(),
            Stat {
                size: 1,
                mtime: now - 2_000,
                dir: false,
            },
        ),
    ]
    .into();
    assert!(hold_unstable(&mut found, &Manifest::new(), now, 2_000));
    assert_eq!(
        found.keys().collect::<Vec<_>>(),
        ["exact.txt", "future.txt"],
        "only the fresh one waits"
    );
    let mut found: BTreeMap<String, Stat> = [(
        "future.txt".to_string(),
        Stat {
            size: 1,
            mtime: now + 1,
            dir: false,
        },
    )]
    .into();
    assert!(!hold_unstable(&mut found, &Manifest::new(), now, 2_000));
}

#[test]
fn a_file_the_scan_skipped_or_never_reached_is_not_tombstoned() {
    let (base, root, mut worker) = scratch_bucket("bucket-keep");
    fs::write(root.join("grows.bin"), "small").unwrap();
    fs::write(root.join("stays.txt"), "s").unwrap();
    fs::write(root.join("other.txt"), "o").unwrap();
    worker.rescan(0);
    assert!(!worker.pairs[0].book.manifest["grows.bin"].deleted);
    // It grows past 1 GiB: skipped by the scan, listed, but not gone.
    fs::File::create(root.join("grows.bin"))
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    worker.rescan(0);
    let manifest = &worker.pairs[0].book.manifest;
    assert!(!manifest["grows.bin"].deleted, "the phone keeps its copy");
    assert!(worker.pairs[0]
        .skipped
        .iter()
        .any(|line| line.starts_with("grows.bin (")));
    // A scan cut short (past the cap) lists fewer files than exist.
    let mut found: BTreeMap<String, Stat> = BTreeMap::new();
    keep_existing(&mut found, manifest, &root);
    assert_eq!(
        found.keys().collect::<Vec<_>>(),
        ["grows.bin", "other.txt", "stays.txt"]
    );
    let mut remote = manifest.clone();
    let before = remote.clone();
    let hash = |_: &str| Some("h".to_string());
    assert!(!apply_scan(
        Side::Desk,
        &mut remote,
        &found,
        &mut |p| hash(p),
        now_ms()
    ));
    assert_eq!(remote, before, "nothing tombstoned by a truncated scan");
    // A really missing file is still a deletion.
    fs::remove_file(root.join("stays.txt")).unwrap();
    worker.rescan(0);
    assert!(worker.pairs[0].book.manifest["stays.txt"].deleted);
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn reading_never_creates_folders() {
    let (base, root, mut worker) = scratch_bucket("bucket-readonly");
    assert_eq!(
        read_path(&root, "ghost/deep/x.txt").unwrap(),
        root.join("ghost/deep/x.txt")
    );
    assert!(!root.join("ghost").exists());
    std::os::unix::fs::symlink(&base, root.join("link")).unwrap();
    assert!(read_path(&root, "link/x").is_err() && read_path(&root, "../x").is_err());
    // A removal of a file in a folder that is not here: no folder made.
    let mut mine = entry("aaa", (1, 0));
    mine.size = 3;
    worker.pairs[0]
        .book
        .manifest
        .insert("ghost/x.txt".into(), mine);
    let packet = folders_packet(
        "manifest",
        json!({"id": BUCKET_ID, "part": 1, "parts": 1, "entries": [
            {"path": "ghost/x.txt", "deleted": true, "vv": {"desk": 1, "phone": 1}},
        ]}),
    );
    worker.on_packet("phone-b", &packet);
    assert!(!root.join("ghost").exists(), "deleting made a folder");
    assert!(
        worker.pairs[0].book.manifest["ghost/x.txt"].deleted || worker.pairs[0].dirty.is_some()
    );
    // A request for a file that is not here makes none either.
    worker.pairs[0]
        .book
        .manifest
        .insert("nowhere/y.txt".into(), entry("bbb", (1, 0)));
    let request = folders_packet(
        "request",
        json!({"id": BUCKET_ID, "paths": ["nowhere/y.txt"]}),
    );
    worker.on_packet("phone-b", &request);
    assert!(!root.join("nowhere").exists());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_manifest_keeps_every_live_entry_and_drops_the_oldest_removals_first() {
    let mut manifest = Manifest::new();
    let long = "\u{10348}".repeat(63);
    for index in 0..MAX_BUCKET_ITEMS {
        manifest.insert(
            format!("{index:05} {long}/{long}/{long}.bin"),
            entry(&"a".repeat(64), (3, 3)),
        );
    }
    for index in 0..MAX_BUCKET_ITEMS {
        let mut gone = tomb((4, 4));
        gone.mtime = index as i64;
        manifest.insert(format!("g{index:05} {long}/{long}/{long}.bin"), gone);
    }
    let packets = manifest_packets(BUCKET_ID, &manifest);
    assert!(packets.len() <= MAX_PARTS, "{}", packets.len());
    let sent: Vec<(String, bool)> = packets
        .iter()
        .flat_map(|packet| {
            packet.body["entries"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .map(|value| {
            (
                value["path"].as_str().unwrap().to_string(),
                value["deleted"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    assert_eq!(
        sent.iter().filter(|(_, gone)| !gone).count(),
        MAX_BUCKET_ITEMS,
        "no live entry lost"
    );
    assert!(sent.len() <= MAX_FILES);
    // The tombstones that fit are the newest.
    let first_gone = sent.iter().find(|(_, gone)| *gone).unwrap();
    assert!(
        first_gone
            .0
            .starts_with(&format!("g{:05}", MAX_BUCKET_ITEMS - 1)),
        "{}",
        &first_gone.0[..8]
    );
    for packet in &packets {
        assert!(crate::device_links::webrtc::fits_on_wire(packet));
    }
}

#[test]
fn conflict_names_drop_what_another_system_refuses_and_measure_in_bytes() {
    let name = conflict_name("docs/plan.md", "Pixel: 9\\x", 0);
    assert_eq!(name, "docs/plan (conflict from Pixel 9x 1970-01-01).md");
    assert!(name_ok(name.rsplit('/').next().unwrap()));
    // 8 two-byte letters are 16 bytes: an extension; 9 are 18: not one.
    let ext8 = "é".repeat(8);
    let ext9 = "é".repeat(9);
    assert!(conflict_name(&format!("a.{ext8}"), "P", 0).ends_with(&format!(").{ext8}")));
    let long = conflict_name(&format!("a.{ext9}"), "P", 0);
    assert!(
        long.ends_with(")") && long.starts_with(&format!("a.{ext9} (conflict")),
        "{long}"
    );
    assert!(name_ok(&long));
}

#[test]
fn a_send_that_could_not_start_frees_its_slot() {
    let (base, _root, mut worker) = scratch_bucket("bucket-sendfailed");
    worker.pairs[0]
        .flight
        .insert("a.txt".into(), Instant::now());
    worker.handle(Message::SendFailed {
        device: "phone-b".into(),
        path: "a.txt".into(),
    });
    assert!(worker.pairs[0].flight.is_empty());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_folder_that_still_holds_something_stays() {
    let mut desk = Replica {
        side: Side::Desk,
        disk: Disk::new(),
        manifest: Manifest::new(),
    };
    let mut phone = Replica {
        side: Side::Phone,
        disk: Disk::new(),
        manifest: Manifest::new(),
    };
    desk.disk.insert("A".into(), DIR.into());
    desk.scan(1);
    sync(&mut phone, &desk);
    phone.scan(2);
    // The phone removes it while the desk (unsynced) put a file in.
    phone.disk.remove("A");
    phone.scan(3);
    desk.disk.insert("A/new.txt".into(), "keep me".into());
    desk.scan(3);
    for round in 4..8 {
        sync(&mut desk, &phone);
        desk.scan(round);
        sync(&mut phone, &desk);
        phone.scan(round);
    }
    assert_eq!(
        desk.disk.get("A/new.txt").map(String::as_str),
        Some("keep me")
    );
    assert_eq!(
        desk.disk, phone.disk,
        "the phone got the folder and the file back"
    );
}

#[test]
fn both_sides_creating_the_same_folder_merge() {
    let mut desk = Replica {
        side: Side::Desk,
        disk: Disk::new(),
        manifest: Manifest::new(),
    };
    let mut phone = Replica {
        side: Side::Phone,
        disk: Disk::new(),
        manifest: Manifest::new(),
    };
    desk.disk.insert("Trips".into(), DIR.into());
    phone.disk.insert("Trips".into(), DIR.into());
    desk.scan(1);
    phone.scan(1);
    let steps = plan(Side::Desk, &desk.manifest, &phone.manifest, "P");
    assert!(
        matches!(steps.as_slice(), [Step::Take { .. }]),
        "one folder, no conflict: {steps:?}"
    );
    sync(&mut desk, &phone);
    sync(&mut phone, &desk);
    assert_eq!(desk.disk, phone.disk);
    assert_eq!(desk.disk.len(), 1);
}

#[test]
fn a_case_only_rename_is_a_rename_and_no_decision_reads_the_clock() {
    let mut local = Manifest::new();
    local.insert("a.txt".into(), entry("aaa", (1, 0)));
    let mut remote = Manifest::new();
    remote.insert("a.txt".into(), tomb((2, 0)));
    remote.insert("A.txt".into(), entry("aaa", (2, 0)));
    let steps = plan(Side::Phone, &local, &remote, "P");
    assert!(
        steps.iter().any(
            |step| matches!(step, Step::Move { from, to, .. } if from == "a.txt" && to == "A.txt")
        ),
        "{steps:?}"
    );
    // Clock skew: mtimes a year apart change nothing.
    let mut skewed = remote.clone();
    for entry in skewed.values_mut() {
        entry.mtime += 365 * 24 * 3600 * 1000;
    }
    assert_eq!(
        plan(Side::Phone, &local, &remote, "P").len(),
        plan(Side::Phone, &local, &skewed, "P").len()
    );
    // Same content on both, concurrent: nothing is sent again.
    let mut ours = Manifest::new();
    ours.insert("p.txt".into(), entry("same", (2, 0)));
    let mut theirs = Manifest::new();
    theirs.insert("p.txt".into(), entry("same", (0, 3)));
    let steps = plan(Side::Desk, &ours, &theirs, "P");
    assert!(matches!(steps.as_slice(), [Step::Take { .. }]), "{steps:?}");
}

#[test]
fn an_empty_folder_from_the_phone_is_created_here() {
    let (base, root, mut worker) = scratch_bucket("bucket-dir-in");
    let packet = folders_packet(
        "manifest",
        json!({"id": BUCKET_ID, "part": 1, "parts": 1, "entries": [
            {"path": "Trips", "dir": true, "vv": {"desk": 0, "phone": 1}},
            {"path": "Trips/Rome", "dir": true, "vv": {"desk": 0, "phone": 1}},
        ]}),
    );
    worker.on_packet("phone-b", &packet);
    assert!(root.join("Trips/Rome").is_dir());
    assert_eq!(worker.pairs[0].phase, Phase::UpToDate, "nothing to fetch");
    let items = worker.pairs[0].view(true).items;
    assert!(items.iter().all(|item| item.dir));
    // A phone-side removal of an empty folder removes it here.
    let removal = folders_packet(
        "manifest",
        json!({"id": BUCKET_ID, "part": 1, "parts": 1, "entries": [
            {"path": "Trips", "dir": true, "vv": {"desk": 0, "phone": 1}},
            {"path": "Trips/Rome", "dir": true, "deleted": true, "vv": {"desk": 0, "phone": 2}},
        ]}),
    );
    worker.on_packet("phone-b", &removal);
    assert!(root.join("Trips").is_dir() && !root.join("Trips/Rome").exists());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_folder_removed_on_the_phone_goes_to_the_trash_as_one_item() {
    let (base, root, mut worker) = scratch_bucket("bucket-remote-trash");
    fs::create_dir_all(root.join("A/B")).unwrap();
    fs::write(root.join("A/one.txt"), "1").unwrap();
    fs::write(root.join("A/B/two.txt"), "2").unwrap();
    worker.rescan(0);
    let manifest = worker.pairs[0].book.manifest.clone();
    assert_eq!(manifest.len(), 4);
    let gone = |path: &str| {
        let mut entry = manifest[path].clone();
        let (size, mtime) = (0, entry.mtime);
        entry.deleted = true;
        entry.size = size;
        entry.mtime = mtime;
        entry.sha256.clear();
        entry.vv.phone = entry.vv.desk + 1;
        json!({"path": path, "dir": entry.dir, "deleted": true,
               "vv": {"desk": entry.vv.desk, "phone": entry.vv.phone}})
    };
    let packet = folders_packet(
        "manifest",
        json!({"id": BUCKET_ID, "part": 1, "parts": 1, "entries": [
            gone("A"), gone("A/B"), gone("A/one.txt"), gone("A/B/two.txt"),
        ]}),
    );
    let before = trashed();
    worker.on_packet("phone-b", &packet);
    assert_eq!(trashed() - before, 1, "the folder, once");
    assert!(!root.join("A").exists());
    assert!(worker.pairs[0]
        .book
        .manifest
        .values()
        .all(|entry| entry.deleted));
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn removing_a_folder_here_is_one_trash_item_and_cancels_its_transfers() {
    let (base, root, mut worker) = scratch_bucket("bucket-remove");
    fs::create_dir_all(root.join("A/B")).unwrap();
    fs::write(root.join("A/one.txt"), "1").unwrap();
    fs::write(root.join("A/B/two.txt"), "2").unwrap();
    fs::write(root.join("keep.txt"), "k").unwrap();
    worker.rescan(0);
    {
        let live = &mut worker.pairs[0];
        live.send_queue.push_back("A/one.txt".into());
        live.flight.insert("A/B/two.txt".into(), Instant::now());
        live.outstanding.push_back("A/B/three.txt".into());
        live.to_request.push_back("A/four.txt".into());
        live.send_queue.push_back("keep.txt".into());
    }
    let before = trashed();
    worker.handle(Message::RemoveItems {
        device: "phone-b".into(),
        paths: vec!["A".into()],
    });
    assert_eq!(trashed() - before, 1, "one item for the whole folder");
    assert!(!root.join("A").exists() && root.join("keep.txt").exists());
    let live = &worker.pairs[0];
    assert_eq!(live.send_queue, ["keep.txt"], "its transfers are dropped");
    assert!(live.flight.is_empty() && live.outstanding.is_empty() && live.to_request.is_empty());
    // The next scan tombstones all four, so the phone removes them.
    worker.rescan(0);
    let manifest = &worker.pairs[0].book.manifest;
    assert!(["A", "A/B", "A/one.txt", "A/B/two.txt"]
        .iter()
        .all(|path| manifest[*path].deleted));
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn bulk_remove_trashes_every_selected_item() {
    let (base, root, mut worker) = scratch_bucket("bucket-bulk");
    fs::create_dir_all(root.join("Folder")).unwrap();
    fs::write(root.join("Folder/in.txt"), "i").unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        fs::write(root.join(name), name).unwrap();
    }
    worker.rescan(0);
    let before = trashed();
    worker.handle(Message::RemoveItems {
        device: "phone-b".into(),
        paths: vec!["a.txt".into(), "Folder".into(), "c.txt".into()],
    });
    assert_eq!(trashed() - before, 3);
    assert!(root.join("b.txt").exists());
    assert!(!root.join("a.txt").exists() && !root.join("c.txt").exists());
    assert!(!root.join("Folder").exists());
    worker.rescan(0);
    let live: Vec<&String> = worker.pairs[0]
        .book
        .manifest
        .iter()
        .filter(|(_, entry)| !entry.deleted)
        .map(|(path, _)| path)
        .collect();
    assert_eq!(live, ["b.txt"]);
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_file_over_1_gb_is_refused_with_the_documented_words() {
    let (base, root, worker) = scratch_bucket("bucket-big");
    let big = base.join("big.bin");
    fs::File::create(&big)
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    let exact = base.join("exact.bin");
    fs::File::create(&exact)
        .unwrap()
        .set_len(MAX_FILE_BYTES)
        .unwrap();
    let mut report = Report::default();
    let adds = collect(
        &[big.clone(), exact.clone()],
        &root,
        "",
        (0, 0),
        &mut report,
    );
    assert_eq!(
        adds.iter().map(|add| add.rel.as_str()).collect::<Vec<_>>(),
        ["exact.bin"]
    );
    assert_eq!(report.too_big, 1);
    assert_eq!(
        report.message().unwrap(),
        "Files over 1 GiB can't go in the Bucket"
    );
    // In the Bucket folder from outside, and from the phone.
    fs::copy(&big, root.join("huge.bin")).unwrap();
    let (found, _, _, skipped) = scan(&worker.pairs[0].pair).unwrap();
    assert!(!found.contains_key("huge.bin"));
    assert_eq!(skipped, ["huge.bin (over 1 GiB)"]);
    let mut packet = NetworkPacket::new(PACKET_TYPE_FOLDERS);
    packet.set(
        "entries",
        json!([{"path": "p.bin", "size": MAX_FILE_BYTES + 1, "mtime": 1,
                "sha256": "a".repeat(64), "vv": {"desk": 0, "phone": 1}}]),
    );
    assert!(manifest_entries(&packet, &worker.pairs[0].pair).is_empty());
    let folder = FolderRef {
        folder: BUCKET_ID.into(),
        relpath: "p.bin".into(),
        mtime: None,
    };
    test_want("phone-b", &folder, &"a".repeat(64), MAX_FILE_BYTES + 1);
    assert!(!accepts(
        "phone-b",
        &folder,
        &"a".repeat(64),
        MAX_FILE_BYTES + 1
    ));
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn an_add_is_limited_in_items_and_bytes_and_the_bucket_is_full_at_10_gb() {
    let base = scratch("bucket-add-limits");
    let root = base.join("bucket");
    fs::create_dir_all(&root).unwrap();
    let outside = base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let many: Vec<PathBuf> = (0..MAX_ADD_ITEMS + 3)
        .map(|index| {
            let path = outside.join(format!("f{index:04}.txt"));
            fs::write(&path, "x").unwrap();
            path
        })
        .collect();
    let mut report = Report::default();
    let adds = collect(&many, &root, "", (0, 0), &mut report);
    assert_eq!((adds.len(), report.over_items), (MAX_ADD_ITEMS, 3));
    assert!(report
        .message()
        .unwrap()
        .contains("Only 500 items can be added at once; 3 were left out"));
    // 5 GiB at once: six 1 GiB files, the sixth left out.
    let big: Vec<PathBuf> = (0..6)
        .map(|index| {
            let path = outside.join(format!("big{index}.bin"));
            fs::File::create(&path)
                .unwrap()
                .set_len(MAX_FILE_BYTES)
                .unwrap();
            path
        })
        .collect();
    let mut report = Report::default();
    let adds = collect(&big, &root, "", (0, 0), &mut report);
    assert_eq!((adds.len(), report.over_bytes), (5, 1));
    // The Bucket would pass 10 GiB.
    let mut report = Report::default();
    let adds = collect(
        &big,
        &root,
        "",
        (BUCKET_FULL_BYTES - 2 * MAX_FILE_BYTES, 10),
        &mut report,
    );
    assert_eq!(adds.len(), 2);
    assert!(report.full);
    assert!(report
        .message()
        .unwrap()
        .contains("The Bucket is full (10 GiB)"));
    // The item cap of a Bucket.
    let mut report = Report::default();
    let adds = collect(
        &many[..5],
        &root,
        "",
        (0, MAX_BUCKET_ITEMS - 2),
        &mut report,
    );
    assert_eq!((adds.len(), report.over_items), (2, 3));
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn adding_a_folder_copies_what_is_in_it_within_the_depth() {
    let (base, root, mut worker) = scratch_bucket("bucket-add-folder");
    let outside = base.join("Holiday");
    fs::create_dir_all(outside.join("Day 1/Raw")).unwrap();
    fs::create_dir_all(outside.join("Empty")).unwrap();
    fs::write(outside.join("Day 1/a.jpg"), "a").unwrap();
    fs::write(outside.join("Day 1/Raw/r.cr2"), "r").unwrap();
    std::os::unix::fs::symlink(outside.join("Day 1/a.jpg"), outside.join("link.jpg")).unwrap();
    worker.handle(Message::Import {
        device: "phone-b".into(),
        sources: vec![outside.clone()],
        into: String::new(),
    });
    assert_eq!(
        fs::read_to_string(root.join("Holiday/Day 1/a.jpg")).unwrap(),
        "a"
    );
    assert!(
        root.join("Holiday/Empty").is_dir(),
        "empty folders are copied"
    );
    assert!(
        !root.join("Holiday/Day 1/Raw").exists(),
        "level 3 is left out"
    );
    assert!(!root.join("Holiday/link.jpg").exists(), "links are skipped");
    let message = take_report().expect("what was left out is said");
    assert!(message.contains("left out (2)"), "{message}");
    // Again: the name is taken, so it is a second folder.
    worker.handle(Message::Import {
        device: "phone-b".into(),
        sources: vec![outside],
        into: String::new(),
    });
    assert!(root.join("Holiday (2)/Empty").is_dir());
    let _ = take_report();
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn new_folder_is_refused_at_level_two_and_names_stay_unique() {
    let (base, root, mut worker) = scratch_bucket("bucket-new-folder");
    worker.handle(Message::NewFolder {
        device: "phone-b".into(),
        parent: String::new(),
    });
    worker.handle(Message::NewFolder {
        device: "phone-b".into(),
        parent: String::new(),
    });
    assert!(root.join("New Folder").is_dir() && root.join("New Folder (2)").is_dir());
    worker.handle(Message::NewFolder {
        device: "phone-b".into(),
        parent: "New Folder".into(),
    });
    assert!(
        root.join("New Folder/New Folder").is_dir(),
        "level 2 is allowed"
    );
    let _ = take_report();
    worker.handle(Message::NewFolder {
        device: "phone-b".into(),
        parent: "New Folder/New Folder".into(),
    });
    assert!(!root.join("New Folder/New Folder/New Folder").exists());
    assert_eq!(
        take_report().unwrap(),
        "Folders can only go 2 levels deep in the Bucket"
    );
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn moving_into_a_folder_that_would_be_too_deep_is_refused_per_item() {
    let (base, root, mut worker) = scratch_bucket("bucket-move");
    fs::create_dir_all(root.join("A/B")).unwrap();
    fs::create_dir_all(root.join("C/D")).unwrap();
    fs::create_dir_all(root.join("E")).unwrap();
    fs::write(root.join("f.txt"), "f").unwrap();
    worker.rescan(0);
    let _ = take_report();
    worker.handle(Message::MoveItems {
        device: "phone-b".into(),
        paths: vec!["f.txt".into(), "E".into(), "C".into()],
        dest: "A/B".into(),
    });
    assert!(root.join("A/B/f.txt").exists(), "a file fits at level 2");
    assert!(!root.join("A/B/E").exists(), "a folder would be level 3");
    assert!(
        root.join("E").is_dir() && root.join("C/D").is_dir(),
        "left where they are"
    );
    assert_eq!(
        take_report().unwrap(),
        "2 items couldn't be moved: they would be more than 2 folders deep"
    );
    // Into a level-1 folder, a folder with no subfolders fits; one with them does not.
    worker.handle(Message::MoveItems {
        device: "phone-b".into(),
        paths: vec!["E".into(), "C".into()],
        dest: "A".into(),
    });
    assert!(root.join("A/E").is_dir());
    assert!(root.join("C/D").is_dir(), "C holds a folder: level 3");
    assert_eq!(
        take_report().unwrap(),
        "1 item couldn't be moved: they would be more than 2 folders deep"
    );
    // Not into itself.
    worker.handle(Message::MoveItems {
        device: "phone-b".into(),
        paths: vec!["A".into()],
        dest: "A/B".into(),
    });
    assert_eq!(take_report().unwrap(), "A folder can't go inside itself");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn one_transfer_is_in_flight_per_direction_and_the_rest_wait() {
    // Sending: the queue fills the slot; an ended transfer frees it.
    let mut queue: VecDeque<String> = (1..=5).map(|n| format!("f{n}")).collect();
    let mut flight = HashMap::new();
    let now = Instant::now();
    assert_eq!(due_sends(&mut queue, &mut flight, &[], now), ["f1"]);
    assert!(
        due_sends(&mut queue, &mut flight, &[], now).is_empty(),
        "full"
    );
    assert_eq!(
        due_sends(&mut queue, &mut flight, &["f1".into()], now),
        ["f2"]
    );
    assert_eq!(
        due_sends(&mut queue, &mut flight, &["f2".into()], now),
        ["f3"]
    );
    assert_eq!(flight.len(), MAX_IN_FLIGHT);
    // A transfer that never showed lapses, so a lost one cannot hold a slot.
    let later = now + FLIGHT_LAPSES + Duration::from_secs(1);
    queue.push_back("f6".into());
    assert_eq!(due_sends(&mut queue, &mut flight, &[], later), ["f4"]);
    // Receiving: only one path is asked for at a time.
    let (base, _root, mut worker) = scratch_bucket("bucket-flight");
    worker.pairs[0].to_request = (1..=6).map(|n| format!("r{n}")).collect();
    worker.request_more(0);
    assert_eq!(worker.pairs[0].outstanding.len(), MAX_IN_FLIGHT);
    assert_eq!(worker.pairs[0].to_request.len(), 5, "the rest are Waiting…");
    worker.request_more(0);
    assert_eq!(worker.pairs[0].outstanding.len(), MAX_IN_FLIGHT);
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_file_still_being_written_waits_until_it_has_held_for_two_seconds() {
    let now = 10_000_000i64;
    let fresh = Stat {
        size: 5,
        mtime: now - 500,
        dir: false,
    };
    let old = Stat {
        size: 5,
        mtime: now - 5_000,
        dir: false,
    };
    let mut found: BTreeMap<String, Stat> = [
        ("download.part2".to_string(), fresh),
        ("settled.txt".to_string(), old),
        ("edited.txt".to_string(), fresh),
        (
            "folder".to_string(),
            Stat {
                size: 0,
                mtime: 0,
                dir: true,
            },
        ),
    ]
    .into();
    let mut manifest = Manifest::new();
    manifest.insert(
        "edited.txt".into(),
        Entry {
            mtime: now - 90_000,
            size: 4,
            ..entry("old", (1, 0))
        },
    );
    assert!(hold_unstable(&mut found, &manifest, now, 2_000));
    assert!(
        !found.contains_key("download.part2"),
        "new and unstable: not yet"
    );
    assert!(found.contains_key("settled.txt") && found.contains_key("folder"));
    assert_eq!(
        found["edited.txt"].mtime,
        now - 90_000,
        "held at the old version"
    );
    // Two seconds later it is stable.
    let mut found: BTreeMap<String, Stat> = [("download.part2".to_string(), fresh)].into();
    assert!(!hold_unstable(&mut found, &manifest, now + 2_000, 2_000));
    assert!(found.contains_key("download.part2"));
    // The worker rescans soon rather than waiting a minute.
    let (base, root, mut worker) = scratch_bucket("bucket-stable");
    TEST_STABLE_MS.with(|window| window.set(STABLE_FOR.as_millis() as i64));
    fs::write(root.join("new.bin"), "being written").unwrap();
    worker.rescan(0);
    assert!(
        worker.pairs[0].book.manifest.is_empty(),
        "too fresh to sync"
    );
    assert!(worker.pairs[0].dirty.is_some(), "scanned again soon");
    TEST_STABLE_MS.with(|window| window.set(0));
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_file_changed_after_it_was_asked_for_is_not_sent_and_is_scanned_again() {
    let (base, root, mut worker) = scratch_bucket("bucket-changed");
    fs::write(root.join("doc.txt"), "one").unwrap();
    std::thread::sleep(Duration::from_millis(5));
    let old = Duration::from_secs(10);
    let file = fs::OpenOptions::new()
        .write(true)
        .open(root.join("doc.txt"))
        .unwrap();
    file.set_modified(SystemTime::now() - old).unwrap();
    worker.rescan(0);
    assert!(worker.pairs[0].book.manifest.contains_key("doc.txt"));
    fs::write(root.join("doc.txt"), "two, longer").unwrap();
    worker.pairs[0].dirty = None;
    worker.pairs[0].send_queue.push_back("doc.txt".into());
    worker.pump_sends(0, &[]);
    let live = &worker.pairs[0];
    assert!(live.flight.is_empty(), "not sent");
    assert!(
        live.dirty.is_some(),
        "scanned again, so the new version is asked for"
    );
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn an_unpaired_phone_loses_its_pairs_and_records_but_not_its_bucket_files() {
    // Ids of this test's own: the refused and wanted records are
    // process-wide, and other tests write "phone-b" into them while this
    // one forgets (and asserts on) every record of its phone.
    let phone = format!("phone-forget-{}", uuid::Uuid::new_v4().simple());
    let other = format!("phone-other-{}", uuid::Uuid::new_v4().simple());
    let base = scratch("bucket-forget");
    let root = base.join("bucket");
    fs::create_dir_all(&root).unwrap();
    let mut worker = bucket_worker_for(&base, &root, &phone);
    worker.ready = [phone.clone()].into();
    fs::write(root.join("keep.txt"), "x").unwrap();
    mark_trash_refused(&phone, "a.txt");
    mark_trash_refused(&other, "a.txt");
    WANTED.lock().unwrap().insert(
        (phone.clone(), BUCKET_ID.into(), "k".into()),
        entry("h", (0, 0)),
    );
    assert!(!worker.pairs.is_empty());
    forget_device(&phone);
    worker.handle(Message::Forget(phone.clone()));
    assert!(worker.pairs.iter().all(|live| live.pair.device != phone));
    assert!(root.join("keep.txt").exists(), "files stay");
    assert!(!replace_anyway_offered(&phone, "a.txt"));
    assert!(replace_anyway_offered(&other, "a.txt"), "only this phone's");
    assert!(WANTED
        .lock()
        .unwrap()
        .keys()
        .all(|(owner, _, _)| *owner != phone));
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn turning_the_bucket_off_keeps_every_file_and_on_again_resends_nothing_identical() {
    let (base, root, mut worker) = scratch_bucket("bucket-off-on");
    fs::create_dir_all(root.join("A")).unwrap();
    fs::write(root.join("A/x.txt"), "x").unwrap();
    worker.handle(Message::Remove {
        device: "phone-b".into(),
        id: BUCKET_ID.into(),
    });
    assert!(root.join("A/x.txt").exists(), "files stay");
    assert!(worker.pairs.is_empty());
    // Both sides hold the same file under unrelated version counters:
    // taken as is, no transfer.
    let mut desk = Manifest::new();
    desk.insert("x.txt".into(), entry("same", (1, 0)));
    let mut phone = Manifest::new();
    phone.insert("x.txt".into(), entry("same", (0, 4)));
    let steps = plan(Side::Desk, &desk, &phone, "P");
    assert!(
        steps.iter().all(|step| matches!(step, Step::Take { .. })),
        "{steps:?}"
    );
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_folders_tile_adds_up_and_its_badge_rolls_up() {
    let item = |path: &str, size: u64, state: ItemState, dir: bool| Item {
        dir,
        path: path.into(),
        size,
        mtime: 0,
        from: Side::Desk,
        state,
    };
    let items = vec![
        item("A", 0, ItemState::Synced, true),
        item("A/B", 0, ItemState::Synced, true),
        item("A/B/x", 100, ItemState::Receiving, false),
        item("A/y", 20, ItemState::Synced, false),
        item("Ab", 7, ItemState::Synced, false),
    ];
    assert_eq!(totals(&items, "A"), (3, 120), "A/B, A/B/x, A/y; not Ab");
    assert_eq!(totals(&items, "A/B"), (1, 100));
    assert_eq!(parent_of("A/B/x"), "A/B");
    assert_eq!(parent_of("x"), "");
    use ItemState::*;
    assert_eq!(rollup([Synced, Synced]), Synced);
    assert_eq!(rollup([Synced, Waiting]), Waiting);
    assert_eq!(rollup([Waiting, Sending]), Sending);
    assert_eq!(rollup([Sending, Receiving, Waiting]), Receiving);
    assert_eq!(rollup([Receiving, Conflict, Synced]), Conflict);
    assert_eq!(rollup([]), Synced);
}

/// S3: hidden/.git/.tmp items are left out at add time and counted; a
/// name differing only by case counts as taken (the phone's file system
/// may fold case).
/// S3: with no trash "Keep the phone's" says why and changes nothing;
/// "Replace Anyway" does it.
#[test]
fn keeping_the_phones_version_without_a_trash_says_so_and_can_be_forced() {
    let (base, root, mut worker) = scratch_bucket("bucket-no-trash");
    fs::write(root.join("nt.txt"), "desk").unwrap();
    fs::write(root.join("nt (conflicted copy).txt"), "phone").unwrap();
    worker.pairs[0].book.conflicts.push(Conflict {
        path: "nt.txt".into(),
        copy: "nt (conflicted copy).txt".into(),
        at: 0,
    });
    TEST_TRASH_FAILS.with(|fails| fails.set(true));
    assert!(
        !replace_anyway_offered("phone-b", "nt.txt"),
        "not before a refusal"
    );
    worker.resolve("phone-b", BUCKET_ID, "nt.txt", Resolve::Phone);
    assert_eq!(fs::read_to_string(root.join("nt.txt")).unwrap(), "desk");
    assert_eq!(worker.pairs[0].book.conflicts.len(), 1, "still a conflict");
    assert!(take_report().is_some_and(|m| m.contains("Replace Anyway")));
    assert!(
        replace_anyway_offered("phone-b", "nt.txt"),
        "offered after the refusal"
    );
    worker.resolve("phone-b", BUCKET_ID, "nt.txt", Resolve::PhoneAnyway);
    assert!(
        !replace_anyway_offered("phone-b", "nt.txt"),
        "and not after"
    );
    assert_eq!(fs::read_to_string(root.join("nt.txt")).unwrap(), "phone");
    TEST_TRASH_FAILS.with(|fails| fails.set(false));
    let _ = fs::remove_dir_all(&base);
}

/// Keeping this computer's version with no trash cannot discard the phone's
/// copy: it stays, and the person is told so.
#[test]
fn keeping_the_desks_version_without_a_trash_says_the_phones_copy_was_kept() {
    let (base, root, mut worker) = scratch_bucket("bucket-no-trash-desk");
    fs::write(root.join("nd.txt"), "desk").unwrap();
    fs::write(root.join("nd (conflicted copy).txt"), "phone").unwrap();
    worker.pairs[0].book.conflicts.push(Conflict {
        path: "nd.txt".into(),
        copy: "nd (conflicted copy).txt".into(),
        at: 0,
    });
    let _ = take_report();
    TEST_TRASH_FAILS.with(|fails| fails.set(true));
    worker.resolve("phone-b", BUCKET_ID, "nd.txt", Resolve::Desk);
    TEST_TRASH_FAILS.with(|fails| fails.set(false));
    assert!(root.join("nd (conflicted copy).txt").exists());
    assert!(take_report().is_some_and(|m| m.contains("kept as a file")));
    let _ = fs::remove_dir_all(&base);
}

/// A folder that cannot be listed is not an empty one: its files are kept
/// in the scan (nothing is tombstoned, so nothing is deleted on the phone);
/// a folder that is really gone still takes its files with it.
#[test]
fn files_in_a_folder_that_cannot_be_listed_are_kept_not_tombstoned() {
    use std::os::unix::fs::PermissionsExt;
    let (base, root, _worker) = scratch_bucket("bucket-unreadable");
    fs::create_dir_all(root.join("A")).unwrap();
    fs::create_dir_all(root.join("B")).unwrap();
    fs::write(root.join("A/f.txt"), "1").unwrap();
    let mut manifest = Manifest::new();
    for path in ["A/f.txt", "B/g.txt"] {
        manifest.insert(path.into(), entry(&"a".repeat(64), (1, 0)));
    }
    fs::set_permissions(root.join("A"), fs::Permissions::from_mode(0o000)).unwrap();
    let listable = fs::read_dir(root.join("A")).is_ok();
    let mut found = BTreeMap::new();
    keep_existing(&mut found, &manifest, &root);
    fs::set_permissions(root.join("A"), fs::Permissions::from_mode(0o755)).unwrap();
    if !listable {
        assert!(found.contains_key("A/f.txt"), "{found:?}");
    }
    // B holds nothing and lists fine: g.txt really is gone.
    assert!(!found.contains_key("B/g.txt"));
    let _ = fs::remove_dir_all(&base);
}

/// C10: a file removed here that the phone changed comes back, and the
/// person is told which.
#[test]
fn a_file_that_comes_back_because_the_phone_changed_it_is_reported() {
    let (base, _root, mut worker) = scratch_bucket("bucket-restored");
    let name = format!("c10-{}.txt", uuid::Uuid::new_v4().simple());
    worker.pairs[0].book.manifest.insert(
        name.clone(),
        Entry {
            deleted: true,
            sha256: String::new(),
            size: 0,
            dir: false,
            mtime: 1,
            vv: Vv { desk: 2, phone: 0 },
        },
    );
    let packet = folders_packet(
        "manifest",
        json!({"id": BUCKET_ID, "part": 1, "parts": 1, "entries": [
            {"path": name, "size": 5, "mtime": 2, "sha256": "a".repeat(64),
             "vv": {"desk": 1, "phone": 1}},
        ]}),
    );
    worker.on_packet("phone-b", &packet);
    let told: Vec<String> = with_reports(|reports| reports.iter().cloned().collect()).unwrap();
    assert!(
        told.iter()
            .any(|message| message.contains(&format!("{name} was restored"))),
        "{told:?}"
    );
    let _ = fs::remove_dir_all(&base);
}

/// S4: "Keep the phone's" trashes this computer's version first.
#[test]
fn keeping_the_phones_version_trashes_the_desks_first() {
    let (base, root, mut worker) = scratch_bucket("bucket-keep-phone");
    fs::write(root.join("a.txt"), "desk").unwrap();
    fs::write(root.join("a (conflicted copy).txt"), "phone").unwrap();
    worker.pairs[0].book.conflicts.push(Conflict {
        path: "a.txt".into(),
        copy: "a (conflicted copy).txt".into(),
        at: 0,
    });
    let before = trashed();
    worker.resolve("phone-b", BUCKET_ID, "a.txt", Resolve::Phone);
    assert_eq!(
        trashed(),
        before + 1,
        "the desk's version went to the trash"
    );
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "phone");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn an_add_skips_what_never_syncs_and_names_ignore_case() {
    let base = scratch("add-hidden");
    let source = base.join("src");
    fs::create_dir_all(source.join(".git")).unwrap();
    fs::write(source.join(".git/config"), "x").unwrap();
    fs::write(source.join(".hidden"), "x").unwrap();
    fs::write(source.join("scratch.tmp"), "x").unwrap();
    fs::write(source.join("keep.txt"), "x").unwrap();
    let root = base.join("bucket");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Photo.JPG"), "x").unwrap();
    let mut report = Report::default();
    let adds = collect(
        std::slice::from_ref(&source),
        &root,
        "",
        (0, 0),
        &mut report,
    );
    let rels: Vec<&str> = adds.iter().map(|add| add.rel.as_str()).collect();
    assert!(rels.contains(&"src/keep.txt"), "{rels:?}");
    assert!(!rels
        .iter()
        .any(|rel| rel.contains(".git") || rel.contains(".hidden")));
    assert_eq!(report.hidden, 3, ".git, .hidden and scratch.tmp");
    let taken = HashSet::new();
    let unique = unique_among(&root, "photo.jpg", &taken);
    assert_eq!(unique.file_name().unwrap(), "photo (2).jpg");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn what_an_add_left_out_is_said_plainly() {
    let report = Report {
        added: 3,
        too_big: 2,
        too_deep: 4,
        bad_names: 1,
        full: true,
        over_items: 5,
        over_bytes: 1,
        not_moved: 2,
        hidden: 6,
        failed: 2,
        restored: vec!["a.txt".into(), "b.txt".into()],
        refused: Some("No.".into()),
    };
    let message = report.message().unwrap();
    for part in [
        "Added 3 items to the Bucket",
        "No.",
        "Files over 1 GiB can't go in the Bucket (2)",
        "Some items were more than 2 folders deep and were left out (4)",
        "Some names can't sync, so they were left out (1)",
        "The Bucket is full (10 GiB)",
        "Only 500 items can be added at once; 5 were left out",
        "Only 5 GiB can be added at once; 1 were left out",
        "2 items couldn't be moved",
        "Hidden and temporary items don't sync, so they were left out (6)",
        "2 items couldn't be copied",
        "a.txt and 1 more were restored",
    ] {
        assert!(message.contains(part), "{part}: {message}");
    }
    assert_eq!(Report::default().message(), None);
    assert_eq!(
        Report {
            added: 1,
            ..Report::default()
        }
        .message()
        .unwrap(),
        "Added 1 item to the Bucket"
    );
}

#[test]
fn pasted_text_and_links_become_files() {
    let now = gtk::glib::DateTime::from_local(2026, 10, 22, 14, 5, 9.0).unwrap();
    let (name, bytes) = pasted_text("https://github.com/derpx06/desklink?tab=1", &now);
    assert_eq!(name, "github.com 2026-10-22.url");
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        "[Desktop Entry]\nType=Link\nName=github.com\nURL=https://github.com/derpx06/desklink?tab=1\n"
    );
    let (name, bytes) = pasted_text("Door code 4471\nBring the keys", &now);
    assert_eq!(name, "Text 2026-10-22 14.05.09.txt");
    assert_eq!(bytes, b"Door code 4471\nBring the keys");
    // Userinfo, ports and IPv6 never leave a name another system refuses.
    for (link, host) in [
        ("https://user:pw@example.com:8080/x", "example.com"),
        ("http://[::1]:3000/", "1"),
        ("http://[2001:db8::7]/p", "2001-db8--7"),
        ("http://192.168.1.2:8000", "192.168.1.2"),
        ("https://@/x", "Link"),
    ] {
        let (name, _) = pasted_text(link, &now);
        assert_eq!(name, format!("{host} 2026-10-22.url"), "{link}");
        assert!(name_ok(&name), "{name}");
    }
    let (name, _) = pasted_text("see https://a.b and more", &now);
    assert!(name.ends_with(".txt"), "a sentence with a link is text");
    assert!(import_bytes("p", "Text.txt", vec![b'x'; MAX_TEXT_ITEM_BYTES + 1], "").is_err());
    assert!(import_bytes("p", "Text.txt", Vec::new(), "").is_err());
    // Refused now, not after a success toast: a name the scan would drop.
    assert_eq!(
        import_bytes("p", "a:b.url", b"x".to_vec(), "").unwrap_err(),
        "That name cannot be used"
    );
    assert!(import_bytes("p", "ok.url", b"x".to_vec(), "A/B/C").is_err());
    assert!(import_bytes("p", "ok.txt", b"x".to_vec(), "").is_ok());
    assert_eq!(clean_name("a:b\\c. ", "x"), "a-b-c");
    assert_eq!(clean_name("...", "x"), "x");
}

#[test]
fn the_workers_refusals_say_what_is_wrong_not_full() {
    let (base, root, mut worker) = scratch_bucket("bucket-bytes-refusal");
    let refuse = |worker: &mut Worker, name: &str| {
        worker.import_bytes("phone-b", name, b"x".to_vec(), "");
        take_report()
    };
    assert_eq!(
        refuse(&mut worker, "a:b.txt").unwrap(),
        "That name cannot be used"
    );
    assert!(!root.join("a:b.txt").exists());
    assert!(refuse(&mut worker, "fine.txt").is_none());
    assert!(root.join("fine.txt").exists());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn each_phone_gets_a_sibling_bucket_never_nested() {
    let home = Path::new("/home/me");
    let first = bucket_root(home, "Pixel 9", &[]);
    assert_eq!(first, home.join("DeskLink Bucket"));
    let second = bucket_root(home, "realme/14", std::slice::from_ref(&first));
    assert_eq!(second, home.join("DeskLink Bucket (realme 14)"));
    let third = bucket_root(home, "realme/14", &[first.clone(), second.clone()]);
    assert_eq!(third, home.join("DeskLink Bucket (realme 14 2)"));
    // A pair inside it, or around it, makes it taken too.
    let around = bucket_root(home, "P", &[home.to_path_buf()]);
    assert_ne!(around, home.join("DeskLink Bucket"));
    assert_eq!(
        unique_in(Path::new("/nonexistent"), "a.txt"),
        Path::new("/nonexistent/a.txt")
    );
}

#[test]
fn the_bucket_is_bookmarked_and_unbookmarked() {
    let base = scratch("bookmarks");
    let file = base.join("gtk-3.0").join("bookmarks");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "file:///home/me/Work Work\n").unwrap();
    TEST_BOOKMARKS.with(|path| path.replace(Some(file.clone())));
    let bucket = Path::new("/home/me/DeskLink Bucket");
    bookmark(bucket, true);
    bookmark(bucket, true);
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "file:///home/me/Work Work\nfile:///home/me/DeskLink%20Bucket DeskLink Bucket\n",
        "added once, other lines untouched"
    );
    bookmark(bucket, false);
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "file:///home/me/Work Work\n"
    );
    TEST_BOOKMARKS.with(|path| path.replace(None));
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn turning_the_bucket_on_makes_its_folder_and_old_pairs_are_dropped() {
    let base = scratch("bucket-on");
    TEST_HOME.with(|home| home.replace(Some(base.clone())));
    TEST_BOOKMARKS.with(|path| path.replace(Some(base.join("bookmarks"))));
    let mut worker = bucket_worker(&base, &base.join("unused"));
    worker.pairs.clear();
    worker.dropped = 2;
    worker.handle(Message::TurnOnBucket {
        device: "phone-b".into(),
    });
    assert!(base.join("DeskLink Bucket").is_dir());
    assert_eq!(worker.dropped, 0, "the note goes once a Bucket is on");
    assert!(fs::read_to_string(base.join("bookmarks"))
        .unwrap()
        .contains("DeskLink%20Bucket"));
    // The phone accepts: the pair is a Bucket.
    worker.on_packet(
        "phone-b",
        &folders_packet("accept", json!({"id": BUCKET_ID})),
    );
    assert!(worker.pairs[0].pair.bucket);
    // A phone's folder offer (not a Bucket) is declined, never listed.
    worker.on_packet(
        "phone-b",
        &folders_packet("offer", json!({"id": "x1", "name": "Camera"})),
    );
    assert!(worker.offered.is_empty());
    // Stored pairs: old folder pairs stop, the Bucket stays.
    let old = Pair {
        id: "old".into(),
        bucket: false,
        ..worker.pairs[0].pair.clone()
    };
    let stored = Pairs {
        version: SCHEMA,
        pairs: vec![old, worker.pairs[0].pair.clone()],
        dropped: 0,
    };
    let (kept, dropped, changed) = keep_buckets(&worker.dir, stored);
    assert_eq!((kept.len(), dropped, changed), (1, 1, true));
    TEST_HOME.with(|home| home.replace(None));
    TEST_BOOKMARKS.with(|path| path.replace(None));
    let _ = fs::remove_dir_all(&base);
}

fn test_pair() -> Pair {
    Pair {
        id: "pair-t".into(),
        name: "t".into(),
        root: PathBuf::from("/nonexistent"),
        device: "phone-t".into(),
        include_hidden: false,
        ignore: vec!["*.log".into()],
        bucket: false,
    }
}

#[test]
fn a_phone_cannot_send_what_this_pair_ignores() {
    let mut packet = NetworkPacket::new(PACKET_TYPE_FOLDERS);
    let entry = |path: &str| {
        json!({"path": path, "size": 1, "mtime": 1, "sha256": "a".repeat(64),
               "vv": {"desk": 0, "phone": 1}})
    };
    packet.set(
        "entries",
        json!([
            entry(".git/hooks/pre-commit"),
            entry("src/.env"),
            entry("build.log"),
            entry("node_modules/x.js"),
            entry("docs/readme.md"),
        ]),
    );
    let kept: Vec<String> = manifest_entries(&packet, &test_pair())
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert_eq!(kept, ["docs/readme.md"]);
    let with_hidden = Pair {
        include_hidden: true,
        ..test_pair()
    };
    assert_eq!(
        manifest_entries(&packet, &with_hidden).len(),
        2,
        ".env too, never .git"
    );
}

fn worker_for(base: &Path, root: &Path) -> Worker {
    let pair = Pair {
        id: "pair-h".into(),
        name: "root".into(),
        root: root.to_path_buf(),
        device: "phone-h".into(),
        include_hidden: false,
        ignore: Vec::new(),
        bucket: false,
    };
    let worker = Worker {
        dir: base.join("state"),
        pairs: vec![Live::new(pair, Book::default())],
        ready: ["phone-h".to_string()].into(),
        asked: HashMap::new(),
        offered: Vec::new(),
        dirty: Default::default(),
        dropped: 0,
    };
    fs::create_dir_all(&worker.dir).unwrap();
    worker
}

/// Deleting the last file with `rm` is deliberate: the folder is the
/// same directory it was, so the deletion goes to the phone.
#[test]
fn removing_the_last_file_with_the_folder_still_there_is_applied() {
    let base = std::env::temp_dir().join(format!("desklink-rm-{}", uuid::Uuid::new_v4()));
    let root = base.join("root");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.txt"), "one").unwrap();
    fs::write(root.join("b.txt"), "two").unwrap();
    let mut worker = worker_for(&base, &root);
    worker.rescan(0);
    // One at a time, then everything gone in one scan too.
    fs::remove_file(root.join("a.txt")).unwrap();
    worker.rescan(0);
    assert_ne!(worker.pairs[0].phase, Phase::EmptyHeld);
    fs::remove_file(root.join("b.txt")).unwrap();
    worker.rescan(0);
    assert_ne!(worker.pairs[0].phase, Phase::EmptyHeld);
    assert!(worker.pairs[0].book.manifest["b.txt"].deleted, "tombstoned");
    assert!(worker.pairs[0].book.manifest["a.txt"].deleted);
    let _ = fs::remove_dir_all(&base);
}

/// A folder that is gone, unreadable, or swapped for another is not a
/// deliberate removal: nothing is tombstoned.
#[test]
fn a_missing_unreadable_or_replaced_folder_is_not_a_deliberate_removal() {
    use std::os::unix::fs::PermissionsExt;
    if !crate::test_privileges::unprivileged(
        "device_links::folders::tests::a_missing_unreadable_or_replaced_folder_is_not_a_deliberate_removal",
    ) {
        return;
    }
    for case in ["missing", "unreadable", "replaced"] {
        let base = std::env::temp_dir().join(format!("desklink-gone-{}", uuid::Uuid::new_v4()));
        let root = base.join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "one").unwrap();
        let mut worker = worker_for(&base, &root);
        worker.rescan(0);
        match case {
            "missing" => fs::remove_dir_all(&root).unwrap(),
            "unreadable" => {
                fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
            }
            _ => {
                fs::remove_dir_all(&root).unwrap();
                fs::create_dir_all(&root).unwrap();
            }
        }
        worker.rescan(0);
        assert!(
            !worker.pairs[0].book.manifest["a.txt"].deleted,
            "{case}: nothing tombstoned"
        );
        assert!(
            matches!(worker.pairs[0].phase, Phase::EmptyHeld | Phase::Problem(_)),
            "{case}: held or a problem, not applied: {:?}",
            worker.pairs[0].phase
        );
        let _ = fs::set_permissions(&root, fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&base);
    }
}

/// The rule itself, for the cases a test cannot make on one disk: another
/// device (a drive mounted over the folder, or gone from it), another
/// inode, or nothing known before.
#[test]
fn an_emptied_folder_is_applied_only_when_it_is_the_same_directory() {
    let here = RootStamp {
        dev: 64768,
        ino: 1234,
    };
    assert!(emptied_on_purpose(Some(here), Some(here)));
    let other_device = RootStamp { dev: 2049, ..here };
    let other_inode = RootStamp { ino: 99, ..here };
    assert!(!emptied_on_purpose(Some(here), Some(other_device)));
    assert!(!emptied_on_purpose(Some(here), Some(other_inode)));
    assert!(!emptied_on_purpose(None, Some(here)), "not known before");
    assert!(
        !emptied_on_purpose(Some(here), None),
        "missing or unreadable"
    );
    assert!(!emptied_on_purpose(None, None));
}

#[test]
fn an_empty_folder_is_held_until_confirmed() {
    let base = std::env::temp_dir().join(format!("desklink-hold-{}", uuid::Uuid::new_v4()));
    let root = base.join("root");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.txt"), "one").unwrap();
    let mut worker = worker_for(&base, &root);
    worker.rescan(0);
    // The folder is swapped for an empty one (a drive that is not
    // mounted): held, nothing tombstoned.
    fs::remove_dir_all(&root).unwrap();
    fs::create_dir_all(&root).unwrap();
    worker.rescan(0);
    assert_eq!(worker.pairs[0].phase, Phase::EmptyHeld);
    assert!(
        !worker.pairs[0].book.manifest["a.txt"].deleted,
        "nothing tombstoned"
    );
    worker.handle(Message::ConfirmEmpty {
        device: "phone-h".into(),
        id: "pair-h".into(),
    });
    worker.rescan(0);
    assert!(
        worker.pairs[0].book.manifest["a.txt"].deleted,
        "confirmed: synced as deleted"
    );
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn without_a_trash_a_deleted_file_is_kept_and_reported() {
    let base = std::env::temp_dir().join(format!("desklink-trash-{}", uuid::Uuid::new_v4()));
    let root = base.join("root");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.txt"), "one").unwrap();
    fs::write(root.join("b.txt"), "two").unwrap();
    let mut worker = worker_for(&base, &root);
    worker.rescan(0);
    let a = worker.pairs[0].book.manifest["a.txt"].clone();
    let packet = folders_packet(
        "manifest",
        json!({"id": "pair-h", "part": 1, "parts": 1, "entries": [
            {"path": "a.txt", "size": 0, "mtime": 9, "sha256": "", "deleted": true,
             "vv": {"desk": a.vv.desk, "phone": 1}}]}),
    );
    TEST_TRASH_FAILS.with(|fails| fails.set(true));
    worker.on_packet("phone-h", &packet);
    TEST_TRASH_FAILS.with(|fails| fails.set(false));
    assert!(root.join("a.txt").exists(), "never deleted for good");
    assert_eq!(worker.pairs[0].phase, Phase::Problem(TRASH_MISSING.into()));
    let _ = fs::remove_dir_all(&base);
}

fn entry(sha: &str, vv: (u64, u64)) -> Entry {
    Entry {
        size: sha.len() as u64,
        mtime: 1_700_000_000_000,
        sha256: sha.to_string(),
        deleted: false,
        dir: false,
        vv: Vv {
            desk: vv.0,
            phone: vv.1,
        },
    }
}

fn tomb(vv: (u64, u64)) -> Entry {
    Entry {
        deleted: true,
        dir: false,
        sha256: String::new(),
        size: 0,
        ..entry("", vv)
    }
}

fn manifest(items: &[(&str, Entry)]) -> Manifest {
    items
        .iter()
        .map(|(path, entry)| (path.to_string(), entry.clone()))
        .collect()
}

#[test]
fn dominance_takes_ties_do_nothing_and_concurrent_edits_conflict() {
    let local = manifest(&[
        ("same", entry("a", (1, 1))),
        ("older", entry("a", (1, 0))),
        ("newer", entry("b", (2, 0))),
        ("both", entry("c", (2, 0))),
        ("deleted-there", entry("d", (1, 0))),
        ("deleted-here", tomb((2, 0))),
    ]);
    let remote = manifest(&[
        ("same", entry("a", (1, 1))),
        ("older", entry("z", (1, 1))),
        ("newer", entry("b0", (1, 0))),
        ("both", entry("q", (1, 1))),
        ("deleted-there", tomb((1, 1))),
        ("deleted-here", entry("e", (1, 1))),
        ("new", entry("n", (0, 1))),
        ("gone", tomb((0, 2))),
    ]);
    let steps = plan(Side::Desk, &local, &remote, "Pixel");
    assert!(steps.contains(&Step::Download {
        path: "older".into(),
        entry: entry("z", (1, 1))
    }));
    assert!(steps.contains(&Step::Delete {
        path: "deleted-there".into(),
        entry: tomb((1, 1))
    }));
    assert!(steps.contains(&Step::Download {
        path: "new".into(),
        entry: entry("n", (0, 1))
    }));
    assert!(steps.contains(&Step::Take {
        path: "gone".into(),
        entry: tomb((0, 2))
    }));
    // Concurrent: both edited → the desk keeps it, a conflict is listed.
    assert!(!steps
        .iter()
        .any(|step| matches!(step, Step::Bump { path, .. } if path == "both")));
    assert!(steps
        .iter()
        .any(|step| matches!(step, Step::Conflict { path, copy }
        if path == "both" && copy == "both (conflict from Pixel 2023-11-14)")));
    // Deleted here, edited there: the edit wins.
    assert!(steps.contains(&Step::Download {
        path: "deleted-here".into(),
        entry: entry("e", (2, 1))
    }));
    assert!(!steps.iter().any(|step| matches!(step,
        Step::Take { path, .. } | Step::Download { path, .. } if path == "same" || path == "newer")));
}

#[test]
fn the_phone_renames_its_own_version_in_a_conflict() {
    let local = manifest(&[("notes.txt", entry("p", (1, 2)))]);
    let remote = manifest(&[("notes.txt", entry("d", (2, 1)))]);
    let steps = plan(Side::Phone, &local, &remote, "Pixel 9");
    assert_eq!(
        steps,
        vec![
            Step::Rename {
                from: "notes.txt".into(),
                to: "notes (conflict from Pixel 9 2023-11-14).txt".into()
            },
            Step::Download {
                path: "notes.txt".into(),
                entry: entry("d", (3, 2))
            }
        ]
    );
}

#[test]
fn renames_and_copies_are_found_by_hash() {
    let local = manifest(&[
        ("a/old.jpg", entry("x", (1, 0))),
        ("keep.jpg", entry("k", (1, 0))),
    ]);
    let remote = manifest(&[
        ("a/old.jpg", tomb((1, 1))),
        ("b/new.jpg", entry("x", (0, 1))),
        ("dup.jpg", entry("k", (0, 1))),
    ]);
    let steps = plan(Side::Desk, &local, &remote, "P");
    assert_eq!(
        steps,
        vec![
            Step::Move {
                from: "a/old.jpg".into(),
                to: "b/new.jpg".into(),
                entry: entry("x", (0, 1)),
                tombstone: tomb((1, 1)),
            },
            Step::Copy {
                from: "keep.jpg".into(),
                to: "dup.jpg".into(),
                entry: entry("k", (0, 1)),
            },
        ]
    );
}

#[test]
fn a_scan_bumps_changes_and_tombstones_what_vanished() {
    let mut manifest = manifest(&[
        (
            "kept",
            Entry {
                size: 1,
                mtime: 5,
                ..entry("k", (1, 0))
            },
        ),
        (
            "touched",
            Entry {
                size: 1,
                mtime: 5,
                ..entry("t", (1, 3))
            },
        ),
        (
            "edited",
            Entry {
                size: 1,
                mtime: 5,
                ..entry("e", (1, 3))
            },
        ),
        ("vanished", entry("v", (0, 2))),
    ]);
    let found: BTreeMap<String, Stat> = [
        (
            "kept",
            Stat {
                size: 1,
                mtime: 5,
                dir: false,
            },
        ),
        (
            "touched",
            Stat {
                size: 1,
                mtime: 9,
                dir: false,
            },
        ),
        (
            "edited",
            Stat {
                size: 2,
                mtime: 9,
                dir: false,
            },
        ),
        (
            "new",
            Stat {
                size: 3,
                mtime: 9,
                dir: false,
            },
        ),
    ]
    .into_iter()
    .map(|(path, stat)| (path.to_string(), stat))
    .collect();
    let mut hashed = Vec::new();
    let mut hash = |path: &str| {
        hashed.push(path.to_string());
        Some(match path {
            "touched" => "t".to_string(),
            other => format!("{other}2"),
        })
    };
    assert!(apply_scan(
        Side::Desk,
        &mut manifest,
        &found,
        &mut hash,
        10_000
    ));
    assert_eq!(
        hashed,
        ["edited", "new", "touched"],
        "only what changed is hashed"
    );
    assert_eq!(manifest["kept"].vv, Vv { desk: 1, phone: 0 });
    assert_eq!(
        manifest["touched"].vv,
        Vv { desk: 1, phone: 3 },
        "same content"
    );
    assert_eq!(manifest["edited"].vv, Vv { desk: 2, phone: 3 });
    assert_eq!(manifest["new"].vv, Vv { desk: 1, phone: 0 });
    assert!(manifest["vanished"].deleted);
    assert_eq!(manifest["vanished"].vv, Vv { desk: 1, phone: 2 });
    // Old tombstones are forgotten.
    let later = 10_000 + (TOMBSTONE_DAYS + 1) * 24 * 3600 * 1000;
    apply_scan(Side::Desk, &mut manifest, &found, &mut |_| None, later);
    assert!(!manifest.contains_key("vanished"));
}

#[test]
fn paths_that_escape_or_are_ignored_never_sync() {
    for bad in [
        "", "/abs", "a//b", "../x", "a/../b", "a/./b", ".", "a\\b", "a/b/", "x\0y",
    ] {
        assert!(validate_relpath(bad).is_err(), "{bad:?}");
    }
    assert!(validate_relpath(&["a"; 32].join("/")).is_ok());
    assert!(
        validate_relpath(&vec!["a"; 33].join("/")).is_err(),
        "too deep"
    );
    assert!(validate_relpath(&"a".repeat(1025)).is_err());
    assert!(validate_relpath("photos/2024/IMG 1.jpg").is_ok());
    assert!(ignored("src/.git/HEAD", true, &[]));
    assert!(ignored("web/node_modules/x.js", false, &[]));
    assert!(ignored("a/b.tmp", false, &[]));
    assert!(ignored(".hidden", false, &[]));
    assert!(!ignored(".hidden", true, &[]));
    assert!(ignored(".desklink-x.tmp", true, &[]));
    assert!(ignored("cache/x.log", false, &["*.log".into()]));
    assert!(ignored("build/out", false, &["build".into()]));
    assert!(!ignored("docs/readme.md", false, &["*.log".into()]));
}

#[test]
fn target_paths_stay_under_the_root_and_never_through_a_link() {
    let root = std::env::temp_dir().join(format!("desklink-folders-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let outside = root.with_extension("outside");
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
    assert!(target_path(&root, "link/x").is_err());
    assert!(target_path(&root, "../x").is_err());
    let made = target_path(&root, "new/dir/file.txt").unwrap();
    assert_eq!(made, root.join("new/dir/file.txt"));
    assert!(root.join("new/dir").is_dir());
    let staged = root.join("staged");
    fs::write(&staged, b"hello").unwrap();
    put_in_place(&staged, &made, 1_700_000_000_000).unwrap();
    assert_eq!(fs::read(&made).unwrap(), b"hello");
    assert_eq!(mtime_ms(&fs::metadata(&made).unwrap()), 1_700_000_000_000);
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&outside);
}

#[test]
fn manifests_split_under_the_bound_and_bad_entries_are_dropped() {
    let mut big = Manifest::new();
    for index in 0..3000 {
        big.insert(
            format!("folder/file-{index:05}.txt"),
            entry(&"a".repeat(64), (1, 0)),
        );
    }
    let packets = manifest_packets("pair-1", &big);
    assert!(packets.len() > 1);
    let mut total = 0;
    for packet in &packets {
        assert!(packet.serialize().unwrap().len() <= MAX_PACKET_BYTES);
        total += manifest_entries(packet, &test_pair()).len();
    }
    assert_eq!(total, 3000);
    let mut bad = NetworkPacket::new(PACKET_TYPE_FOLDERS);
    bad.set(
        "entries",
        json!([
            {"path": "../etc/passwd", "size": 1, "mtime": 1, "sha256": "a".repeat(64), "vv": {"desk": 1, "phone": 0}},
            {"path": "ok.txt", "size": 1, "mtime": 1, "sha256": "zz", "vv": {"desk": 1, "phone": 0}},
            {"path": "fine.txt", "size": 1, "mtime": 1, "sha256": "A".repeat(64), "vv": {"desk": 1, "phone": 0}},
        ]),
    );
    let entries = manifest_entries(&bad, &test_pair());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1.sha256, "a".repeat(64), "lowercased");
}

#[test]
fn conflict_names_keep_the_extension_and_stay_valid() {
    assert_eq!(
        conflict_name("docs/plan.final.md", "Pixel/9", 0),
        "docs/plan.final (conflict from Pixel9 1970-01-01).md"
    );
    assert_eq!(
        conflict_name(".env", "P", 0),
        ".env (conflict from P 1970-01-01)"
    );
    let long = conflict_name(&format!("{}.txt", "x".repeat(250)), "P", 0);
    assert!(long.len() <= 255 && validate_relpath(&long).is_ok());
}

type Disk = BTreeMap<String, String>;
/// A folder on a simulated disk.
const DIR: &str = "\u{0}dir";
struct Replica {
    side: Side,
    disk: Disk,
    manifest: Manifest,
}
impl Replica {
    fn scan(&mut self, now: i64) {
        let found: BTreeMap<String, Stat> = self
            .disk
            .iter()
            .map(|(path, content)| {
                let dir = content == DIR;
                (
                    path.clone(),
                    Stat {
                        size: if dir { 0 } else { content.len() as u64 },
                        mtime: if dir { 0 } else { content_mtime(content) },
                        dir,
                    },
                )
            })
            .collect();
        let disk = self.disk.clone();
        apply_scan(
            self.side,
            &mut self.manifest,
            &found,
            &mut |path| disk.get(path).filter(|c| *c != DIR).map(|c| sha(c)),
            now,
        );
    }
}
fn sha(content: &str) -> String {
    format!(
        "{:x}",
        content
            .bytes()
            .fold(7u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b)))
    )
}
fn content_mtime(content: &str) -> i64 {
    // As a real disk: each write has its own time (here, a hash).
    (u64::from_str_radix(&sha(content), 16).unwrap_or(0) >> 2) as i64
}
/// Applies `local`'s plan; downloads read the other side's disk.
fn sync(local: &mut Replica, remote: &Replica) {
    let steps = plan(local.side, &local.manifest, &remote.manifest, "P");
    let mut folder_deletes: Vec<(String, Entry)> = Vec::new();
    for step in steps {
        match step {
            Step::Take { path, entry } => {
                local.manifest.insert(path, entry);
            }
            Step::Bump { path, vv } => {
                if let Some(entry) = local.manifest.get_mut(&path) {
                    entry.vv = vv;
                }
            }
            Step::Delete { path, entry } if entry.dir => folder_deletes.push((path, entry)),
            Step::Delete { path, entry } => {
                local.disk.remove(&path);
                local.manifest.insert(path, entry);
            }
            Step::Download { path, entry } if entry.dir => {
                local.disk.insert(path.clone(), DIR.to_string());
                local.manifest.insert(path, entry);
            }
            Step::Download { path, entry } => {
                // The holder's file must still be that version.
                if let Some(content) = remote.disk.get(&path).filter(|c| sha(c) == entry.sha256) {
                    local.disk.insert(path.clone(), content.clone());
                    local.manifest.insert(
                        path,
                        Entry {
                            mtime: content_mtime(content),
                            ..entry
                        },
                    );
                }
            }
            Step::Move {
                from,
                to,
                entry,
                tombstone,
            } => {
                if let Some(content) = local.disk.remove(&from) {
                    local.disk.insert(to.clone(), content);
                }
                local.manifest.insert(from, tombstone);
                local.manifest.insert(to, entry);
            }
            Step::Copy { from, to, entry } => {
                if let Some(content) = local.disk.get(&from).cloned() {
                    local.disk.insert(to.clone(), content);
                }
                local.manifest.insert(to, entry);
            }
            Step::Rename { from, to } => {
                if let Some(content) = local.disk.remove(&from) {
                    local.disk.insert(to, content);
                }
            }
            Step::Conflict { .. } => {}
        }
    }
    // Folders last, deepest first, only when empty; a kept one wins.
    folder_deletes.sort_by_key(|(path, _)| std::cmp::Reverse(path.len()));
    for (path, entry) in folder_deletes {
        let below = format!("{path}/");
        if local.disk.keys().any(|other| other.starts_with(&below)) {
            if let Some(ours) = local.manifest.get(&path).cloned() {
                let vv = ours.vv.max(entry.vv).bumped(local.side);
                local.manifest.insert(path, Entry { vv, ..ours });
            }
        } else {
            local.disk.remove(&path);
            local.manifest.insert(path, entry);
        }
    }
}

/// The worker naps long unless a Bucket's own phone is connected: a
/// Bucket with its phone away does not wake the computer twice a second.
#[test]
fn the_worker_naps_long_while_no_bucket_phone_is_connected() {
    let pair = Pair {
        id: "pair-n".into(),
        name: "root".into(),
        root: std::env::temp_dir(),
        device: "phone-n".into(),
        include_hidden: false,
        ignore: Vec::new(),
        bucket: false,
    };
    let pairs = vec![Live::new(pair, Book::default())];
    let long = Duration::from_secs(5);
    assert_eq!(worker_nap(&[], &HashSet::new()), long);
    assert_eq!(worker_nap(&pairs, &HashSet::new()), long);
    assert_eq!(worker_nap(&pairs, &["other".to_string()].into()), long);
    assert_eq!(
        worker_nap(&pairs, &["phone-n".to_string()].into()),
        Duration::from_millis(500)
    );
}

/// The worker end to end, without a phone: scan, a phone's manifest, a
/// deletion, a requested file arriving and put in place.
#[test]
fn the_worker_scans_plans_and_places_files() {
    let base = std::env::temp_dir().join(format!("desklink-sync-{}", uuid::Uuid::new_v4()));
    let root = base.join("root");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("a.txt"), "one").unwrap();
    fs::write(root.join("sub/b.txt"), "two").unwrap();
    fs::write(root.join(".hidden"), "x").unwrap();
    let pair = Pair {
        id: "pair-w".into(),
        name: "root".into(),
        root: root.clone(),
        device: "phone-w".into(),
        include_hidden: false,
        ignore: Vec::new(),
        bucket: false,
    };
    let mut worker = Worker {
        dir: base.join("state"),
        pairs: vec![Live::new(pair, Book::default())],
        ready: ["phone-w".to_string()].into(),
        asked: HashMap::new(),
        offered: Vec::new(),
        dirty: Default::default(),
        dropped: 0,
    };
    fs::create_dir_all(&worker.dir).unwrap();
    worker.rescan(0);
    let manifest = &worker.pairs[0].book.manifest;
    assert_eq!(
        manifest.keys().collect::<Vec<_>>(),
        ["a.txt", "sub/b.txt"],
        "hidden skipped"
    );
    let a = manifest["a.txt"].clone();
    let three = "three";
    let three_sha = sha256_of(&{
        let path = base.join("three");
        fs::write(&path, three).unwrap();
        path
    })
    .unwrap();
    let remote = json!([
        {"path": "a.txt", "size": 0, "mtime": 9, "sha256": "", "deleted": true,
         "vv": {"desk": a.vv.desk, "phone": 1}},
        {"path": "c.txt", "size": 5, "mtime": 1_700_000_000_000i64, "sha256": three_sha,
         "vv": {"desk": 0, "phone": 1}},
    ]);
    let packet = folders_packet(
        "manifest",
        json!({"id": "pair-w", "part": 1, "parts": 1, "entries": remote}),
    );
    worker.on_packet("phone-w", &packet);
    assert!(
        !root.join("a.txt").exists(),
        "deleted on the phone, so here"
    );
    assert_eq!(worker.pairs[0].phase, Phase::Syncing(1));
    let folder = FolderRef {
        folder: "pair-w".into(),
        relpath: "c.txt".into(),
        mtime: None,
    };
    assert!(accepts("phone-w", &folder, &three_sha, 5));
    assert!(!accepts("someone-else", &folder, &three_sha, 5));
    let stash = base.join("stash");
    fs::write(&stash, three).unwrap();
    worker.handle(Message::Arrived {
        device: "phone-w".into(),
        folder: folder.clone(),
        stash: stash.clone(),
        sha256: three_sha.clone(),
        size: 5,
    });
    assert_eq!(fs::read_to_string(root.join("c.txt")).unwrap(), three);
    assert_eq!(worker.pairs[0].phase, Phase::UpToDate);
    assert!(!accepts("phone-w", &folder, &three_sha, 5), "asked once");
    // A rescan sees nothing new: our own write is not a change.
    assert!(!apply_scan(
        Side::Desk,
        &mut worker.pairs[0].book.manifest.clone(),
        &scan(&worker.pairs[0].pair).unwrap().0,
        &mut |_| None,
        now_ms()
    ));
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_concurrent_edit_keeps_both_versions_on_both_sides() {
    let mut desk = Replica {
        side: Side::Desk,
        disk: Disk::new(),
        manifest: Manifest::new(),
    };
    let mut phone = Replica {
        side: Side::Phone,
        disk: Disk::new(),
        manifest: Manifest::new(),
    };
    desk.disk.insert("notes.txt".into(), "base".into());
    desk.scan(1);
    sync(&mut phone, &desk);
    assert_eq!(phone.disk["notes.txt"], "base");
    desk.disk.insert("notes.txt".into(), "desk edit".into());
    phone.disk.insert("notes.txt".into(), "phone edit".into());
    desk.scan(2);
    phone.scan(2);
    for _ in 0..3 {
        sync(&mut desk, &phone);
        desk.scan(3);
        sync(&mut phone, &desk);
        phone.scan(3);
    }
    assert_eq!(desk.disk, phone.disk);
    assert_eq!(
        desk.disk["notes.txt"], "desk edit",
        "the desk's keeps the name"
    );
    let copies: Vec<&String> = desk.disk.values().filter(|c| *c == "phone edit").collect();
    assert_eq!(copies.len(), 1, "the phone's is kept as a conflict copy");
}

/// Property test: two replicas edit, delete, rename and create at
/// random; after sync rounds both hold the same files.
#[test]
fn random_edits_on_both_sides_converge() {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move |bound: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % bound
    };
    for round in 0..300 {
        let mut desk = Replica {
            side: Side::Desk,
            disk: Disk::new(),
            manifest: Manifest::new(),
        };
        let mut phone = Replica {
            side: Side::Phone,
            disk: Disk::new(),
            manifest: Manifest::new(),
        };
        let mut now = 1_000;
        for _ in 0..6 {
            for replica in [&mut desk, &mut phone] {
                for _ in 0..next(4) {
                    let path = format!("d{}/f{}", next(2), next(4));
                    match next(4) {
                        0 => {
                            replica.disk.remove(&path);
                        }
                        1 => {
                            if let Some((from, content)) = replica
                                .disk
                                .iter()
                                .next()
                                .map(|(p, c)| (p.clone(), c.clone()))
                            {
                                replica.disk.remove(&from);
                                replica.disk.insert(path, content);
                            }
                        }
                        _ => {
                            replica
                                .disk
                                .insert(path, format!("v{}-{round}", next(1000)));
                        }
                    }
                }
                now += 1;
                replica.scan(now);
            }
            // A few rounds of exchange, as the manifests go back and forth.
            for _ in 0..4 {
                sync(&mut desk, &phone);
                desk.scan(now);
                sync(&mut phone, &desk);
                phone.scan(now);
            }
            assert_eq!(
                desk.disk, phone.disk,
                "round {round}: the files differ\ndesk {:?}\nphone {:?}",
                desk.manifest, phone.manifest
            );
        }
    }
}

// ── W3 — case clashes ──────────────────────────────────────────────────

/// Names that differ only in case, files and folders alike: the one that
/// sorts first in byte order syncs; the others (and what is below them)
/// are listed, never offered.
#[test]
fn a_scan_syncs_the_first_of_names_that_differ_only_in_case() {
    let (_base, root, worker) = scratch_bucket("bucket-case-scan");
    for name in ["A.txt", "a.txt", "B.txt", "É.txt", "é.txt"] {
        fs::write(root.join(name), name).unwrap();
    }
    fs::create_dir_all(root.join("Dir")).unwrap();
    fs::create_dir_all(root.join("dir/sub")).unwrap();
    fs::write(root.join("Dir/x.txt"), "x").unwrap();
    fs::write(root.join("dir/y.txt"), "y").unwrap();
    fs::write(root.join("dir/sub/z.txt"), "z").unwrap();
    // The same name in two folders is not a clash.
    fs::write(root.join("Dir/a.txt"), "1").unwrap();
    fs::write(root.join("Dir/A.TXT"), "2").unwrap();
    let (found, _, _, skipped) = scan(&worker.pairs[0].pair).unwrap();
    assert_eq!(
        found.keys().map(String::as_str).collect::<Vec<_>>(),
        ["A.txt", "B.txt", "Dir", "Dir/A.TXT", "Dir/x.txt", "É.txt"],
        "Dir/a.txt lost to Dir/A.TXT, nothing below `dir` is listed"
    );
    let clash = |name: &str| format!("{name} (another file differs only in capital letters)");
    assert_eq!(
        skipped,
        [
            clash("Dir/a.txt"),
            clash("a.txt"),
            clash("dir"),
            clash("é.txt")
        ],
    );
}

/// The phone's manifest can hold two paths that differ only in case (its
/// own scan or an older build): the later is not synced here either, and
/// a tombstone is never a clash.
#[test]
fn a_manifest_with_two_paths_that_differ_only_in_case_loses_the_later() {
    let folder = |vv| Entry {
        dir: true,
        sha256: String::new(),
        size: 0,
        ..entry("", vv)
    };
    let mut remote = manifest(&[
        ("A.txt", entry("a", (0, 1))),
        ("a.txt", entry("b", (0, 1))),
        ("Dir", folder((0, 1))),
        ("dir", folder((0, 1))),
        ("dir/y.txt", entry("y", (0, 1))),
        ("Dir/x.txt", entry("x", (0, 1))),
        ("B.txt", entry("c", (0, 1))),
        ("b.txt", tomb((0, 2))),
    ]);
    let left_out = drop_case_clashes(&mut remote, |entry| !entry.deleted);
    assert_eq!(left_out, ["a.txt", "dir"]);
    assert_eq!(
        remote.keys().map(String::as_str).collect::<Vec<_>>(),
        ["A.txt", "B.txt", "Dir", "Dir/x.txt", "b.txt"],
        "the tombstone stays"
    );
    // Nothing is asked for under the left-out names.
    let steps = plan(Side::Desk, &Manifest::new(), &remote, "Phone");
    assert!(steps.iter().all(|step| !matches!(
        step,
        Step::Download { path, .. } if path == "a.txt" || path.starts_with("dir")
    )));
}

/// The desk's own copy of the later name is listed as not synced and is
/// never an item, so it cannot read "Waiting…"; once the first name is
/// gone the other syncs.
#[test]
fn the_later_name_is_not_synced_never_waiting_and_syncs_once_alone() {
    let (_base, root, mut worker) = scratch_bucket("bucket-case-ui");
    fs::write(root.join("A.txt"), "1").unwrap();
    fs::write(root.join("a.txt"), "2").unwrap();
    worker.rescan(0);
    for online in [false, true] {
        let view = worker.pairs[0].view(online);
        let paths: Vec<&str> = view.items.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, ["A.txt"], "online: {online}");
        assert_eq!(
            view.skipped,
            ["a.txt (another file differs only in capital letters)"]
        );
    }
    assert!(!worker.pairs[0].book.manifest.contains_key("a.txt"));
    // The manifest sent to the phone never names it.
    let sent = manifest_packets("m", &worker.pairs[0].book.manifest);
    assert!(!format!("{:?}", sent.iter().map(|p| &p.body).collect::<Vec<_>>()).contains("a.txt"));
    fs::remove_file(root.join("A.txt")).unwrap();
    worker.rescan(0);
    let view = worker.pairs[0].view(true);
    assert!(view.skipped.is_empty());
    assert!(view
        .items
        .iter()
        .any(|item| item.path == "a.txt" && !item.dir));
}

/// The vector both apps pin: these names in one directory.
const CASE_VECTOR: [&str; 7] = ["a.txt", "A.txt", "É", "é", "Dir", "dir", "b.txt"];
const CASE_WINNERS: [&str; 4] = ["A.txt", "É", "Dir", "b.txt"];
const CASE_LOSERS: [&str; 3] = ["a.txt", "é", "dir"];

/// Golden: the shared case-clash vector, from a disk scan and from a
/// manifest alike.
#[test]
fn the_shared_case_clash_vector_gives_the_same_winners_everywhere() {
    let (_base, root, worker) = scratch_bucket("bucket-case-vector");
    for name in CASE_VECTOR {
        fs::write(root.join(name), name).unwrap();
    }
    let (found, _, _, skipped) = scan(&worker.pairs[0].pair).unwrap();
    let winners: Vec<&str> = CASE_VECTOR
        .into_iter()
        .filter(|name| found.contains_key(*name))
        .collect();
    assert_eq!(winners, CASE_WINNERS);
    assert_eq!(found.len(), CASE_WINNERS.len());
    let mut listed = skipped.clone();
    listed.sort();
    let mut expected: Vec<String> = CASE_LOSERS
        .iter()
        .map(|name| format!("{name} (another file differs only in capital letters)"))
        .collect();
    expected.sort();
    assert_eq!(listed, expected);
    // The same, as a received manifest (and in any order of arrival).
    let mut remote: Manifest = CASE_VECTOR
        .iter()
        .map(|name| (name.to_string(), entry(name, (0, 1))))
        .collect();
    let mut left_out = left_out_of_remote(&mut remote, &Manifest::new());
    let winners: Vec<&str> = CASE_VECTOR
        .into_iter()
        .filter(|name| remote.contains_key(*name))
        .collect();
    assert_eq!(winners, CASE_WINNERS);
    left_out.sort();
    let mut losers = CASE_LOSERS.map(String::from).to_vec();
    losers.sort();
    assert_eq!(left_out, losers);
}

/// A phone path that clashes in case with one the desk already has is
/// never downloaded, whichever name would sort first; the desk's entry
/// and file are left alone and the phone's is listed as not synced.
#[test]
fn a_phone_path_that_clashes_with_a_desk_path_is_never_downloaded() {
    // (desk has, phone sends): the phone's loses, then the phone's would win.
    for (desk_has, phone_sends) in [("A.txt", "a.txt"), ("a.txt", "A.txt")] {
        let (_base, root, mut worker) = scratch_bucket("bucket-case-remote");
        fs::write(root.join(desk_has), "mine").unwrap();
        fs::write(root.join("other.txt"), "o").unwrap();
        worker.rescan(0);
        let ours = worker.pairs[0].book.manifest.clone();
        assert!(!ours[desk_has].deleted);
        let phone = manifest(&[
            (phone_sends, entry(&"1".repeat(64), (0, 1))),
            ("other.txt", ours["other.txt"].clone()),
            ("new.txt", entry(&"2".repeat(64), (0, 1))),
        ]);
        for packet in manifest_packets(BUCKET_ID, &phone) {
            worker.on_manifest("phone-b", BUCKET_ID, &packet);
        }
        let live = &worker.pairs[0];
        let manifest = &live.book.manifest;
        assert_eq!(manifest[desk_has], ours[desk_has], "{desk_has} untouched");
        assert!(
            !manifest.contains_key(phone_sends),
            "{phone_sends} not taken"
        );
        assert_eq!(fs::read(root.join(desk_has)).unwrap(), b"mine");
        assert!(!root.join(phone_sends).exists());
        let wanted: Vec<&str> = live.to_request.iter().map(|p| p.as_str()).collect();
        assert!(
            !wanted.contains(&phone_sends) && !live.outstanding.iter().any(|p| p == phone_sends),
            "{phone_sends} is never asked for: {wanted:?}"
        );
        assert!(
            live.view(true)
                .skipped
                .contains(&format!("{phone_sends} ({CASE_CLASH})")),
            "{:?}",
            live.view(true).skipped
        );
        assert!(
            !live
                .view(true)
                .items
                .iter()
                .any(|item| item.path == phone_sends),
            "and it is no item, so never Waiting"
        );
        // What does not clash is still taken.
        assert!(wanted.contains(&"new.txt") || live.outstanding.iter().any(|p| p == "new.txt"));
    }
}

/// The file changed on the phone while it was being fetched, and was asked
/// for again: the old copy arriving must not take the newer request away.
#[test]
fn a_stale_arrival_keeps_the_newer_request() {
    let device = format!("stale-{}", uuid::Uuid::new_v4().simple());
    let wanted = FolderRef {
        folder: "bucket".into(),
        relpath: "docs/a.txt".into(),
        mtime: None,
    };
    let newer = "b".repeat(64);
    test_want(&device, &wanted, &newer, 10);
    let key = (
        device.clone(),
        wanted.folder.clone(),
        wanted.relpath.clone(),
    );
    assert!(take_wanted_matching(&key, &"a".repeat(64), 10).is_none());
    assert!(
        accepts(&device, &wanted, &newer, 10),
        "the newer request stays"
    );
    assert!(take_wanted_matching(&key, &newer, 10).is_some());
    assert!(!accepts(&device, &wanted, &newer, 10));
}
