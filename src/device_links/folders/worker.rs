//! The folder worker thread: owns every pair, routes [`Message`]s, publishes what
//! the window shows, ticks, rescans, and runs copies and sends on their lanes.
//!
//! `Worker` is one type whose methods are split by responsibility: this file
//! (life cycle, messages, ticks), `bucket` (Bucket operations) and `engine`
//! (sync). The three call each other's methods; that is one type in three
//! files, not three modules leaning on each other.
//!
//! Sibling edges: `bucket`, `engine`, `state`, `store`, `scan`, `disk`, `model`,
//! `wire`. Outside: `webrtc::transfer_board`, `crate::guard`, gio.

use super::*;

// ── The worker ─────────────────────────────────────────────────────────────

/// A pair at run time.
pub(super) struct Live {
    pub(super) pair: Pair,
    pub(super) book: Book,
    pub(super) phase: Phase,
    /// Watches its directories; a change marks it dirty.
    monitors: HashMap<PathBuf, gtk::gio::FileMonitor>,
    pub(super) dirty: Option<Instant>,
    scanned: Option<Instant>,
    /// The other side's manifest, being assembled: parts received.
    pub(super) incoming: BTreeMap<u32, Vec<(String, Entry)>>,
    /// The other side lists this pair.
    pub(super) on_phone: bool,
    /// Asked of the other side, not yet arrived.
    pub(super) outstanding: VecDeque<String>,
    /// Waiting to be asked (beyond [`MAX_OUTSTANDING`]).
    pub(super) to_request: VecDeque<String>,
    /// The user confirmed an empty folder is meant: the next scan applies.
    pub(super) allow_empty: bool,
    /// Which directory the root was at the last scan that held files: an
    /// emptied root that is still that directory is a deliberate removal; a
    /// different one (a drive unmounted, a folder swapped) is held.
    root_stamp: Option<RootStamp>,
    /// The other side's live files (path → sha256), from its last manifest.
    pub(super) remote: HashMap<String, String>,
    /// Files the other side asked for, and their sha256: sending.
    pub(super) sending: HashMap<String, String>,
    /// Files asked of the other side: receiving.
    pub(super) receiving: BTreeMap<String, Entry>,
    /// What the Bucket does not sync, with why (too deep, a name that can't
    /// sync, over 1 GiB, a link).
    pub(super) skipped: Vec<String>,
    /// The other side's paths left out of its last manifest because they
    /// differ only in case from another (see [`CASE_CLASH`]).
    pub(super) remote_clashes: Vec<String>,
    /// F8c: files the other side asked for, waiting for a free slot of
    /// [`MAX_IN_FLIGHT`].
    pub(super) send_queue: VecDeque<String>,
    /// Files being sent now, and since when.
    pub(super) flight: HashMap<String, Instant>,
}

pub(super) struct Worker {
    pub(super) dir: PathBuf,
    pub(super) pairs: Vec<Live>,
    pub(super) ready: HashSet<String>,
    /// Pairs offered by this side, waiting for the phone: (device, id) →
    /// (root, a Bucket, when).
    pub(super) asked: HashMap<(String, String), (PathBuf, bool, Instant)>,
    /// Offers from phones, waiting here.
    pub(super) offered: Vec<(Offer, Instant)>,
    pub(super) dirty: std::rc::Rc<std::cell::RefCell<HashSet<String>>>,
    /// Old folder pairs dropped (see [`Pairs::dropped`]).
    pub(super) dropped: u32,
}

/// Loads the pairs from `dir` (`<config>/folders`) and starts the worker.
pub(crate) fn start(dir: PathBuf) {
    let (sender, receiver) = mpsc::sync_channel(QUEUE);
    if queue().set(sender).is_err() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("desklink-folders".into())
        .spawn(move || {
            let context = gtk::glib::MainContext::new();
            let _ = context.with_thread_default(|| run(&context, dir, &receiver));
        });
}

/// The Bucket is the only pair (F8b). Folder pairs of the a4a4fdf build
/// stop syncing: their files stay where they are; only DeskLink's record of
/// them goes. The Buckets, how many pairs were dropped (in all), and whether
/// the stored list changed.
pub(super) fn keep_buckets(dir: &Path, stored: Pairs) -> (Vec<Pair>, u32, bool) {
    let (buckets, legacy): (Vec<Pair>, Vec<Pair>) = stored
        .pairs
        .into_iter()
        .partition(|pair| pair.bucket && pair.id == BUCKET_ID);
    for pair in &legacy {
        let _ = fs::remove_file(dir.join(book_file(&pair.device, &pair.id)));
    }
    (
        buckets,
        stored.dropped.saturating_add(legacy.len() as u32),
        !legacy.is_empty(),
    )
}

fn run(context: &gtk::glib::MainContext, dir: PathBuf, messages: &Receiver<Message>) {
    let _ = fs::create_dir_all(&dir);
    let stored: Pairs = load(&dir, PAIRS_FILE, PAIRS_AAD);
    let mut worker = Worker {
        pairs: Vec::new(),
        ready: HashSet::new(),
        asked: HashMap::new(),
        offered: Vec::new(),
        dirty: Default::default(),
        dropped: 0,
        dir,
    };
    let (buckets, dropped, changed) = keep_buckets(&worker.dir, stored);
    worker.dropped = dropped;
    for pair in buckets {
        let book: Book = load(&worker.dir, &book_file(&pair.device, &pair.id), BOOK_AAD);
        worker.pairs.push(Live::new(pair, book));
    }
    if changed {
        worker.save_pairs();
    }
    worker.publish();
    loop {
        let nap = worker_nap(&worker.pairs, &worker.ready);
        match messages.recv_timeout(nap) {
            Ok(message) => worker.handle(message),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while context.pending() {
            context.iteration(false);
        }
        worker.tick();
    }
}

/// A pair's state file: per phone, as each phone's Bucket has id `bucket`.
/// How long the Bucket worker waits for a message: twice a second while a
/// Bucket's phone is connected (file changes, transfers), every five seconds
/// otherwise, when only a message (a phone connecting, a setting) can matter
/// and file changes simply wait for the phone.
pub(super) fn worker_nap(pairs: &[Live], ready: &HashSet<String>) -> Duration {
    if pairs.iter().any(|live| ready.contains(&live.pair.device)) {
        Duration::from_millis(500)
    } else {
        Duration::from_secs(5)
    }
}

impl Live {
    pub(super) fn new(pair: Pair, book: Book) -> Self {
        Self {
            pair,
            book,
            phase: Phase::Waiting,
            monitors: HashMap::new(),
            dirty: None,
            scanned: None,
            incoming: BTreeMap::new(),
            on_phone: true,
            outstanding: VecDeque::new(),
            to_request: VecDeque::new(),
            allow_empty: false,
            root_stamp: None,
            remote: HashMap::new(),
            sending: HashMap::new(),
            receiving: BTreeMap::new(),
            skipped: Vec::new(),
            remote_clashes: Vec::new(),
            send_queue: VecDeque::new(),
            flight: HashMap::new(),
        }
    }

    pub(super) fn view(&self, _online: bool) -> PairView {
        PairView {
            pair: self.pair.clone(),
            phase: self.phase.clone(),
            files: self
                .book
                .manifest
                .values()
                .filter(|entry| !entry.deleted)
                .count(),
            last_sync: self.book.last_sync,
            conflicts: self.book.conflicts.clone(),
            items: if self.pair.bucket {
                self.items()
            } else {
                Vec::new()
            },
            bytes: self
                .book
                .manifest
                .values()
                .filter(|entry| !entry.deleted)
                .map(|entry| entry.size)
                .sum(),
            skipped: self
                .skipped
                .iter()
                .chain(&self.remote_clashes)
                .take(MAX_SKIPPED_SHOWN)
                .cloned()
                .collect(),
        }
    }

    /// F8b: the Bucket's items, newest first, each with its state.
    fn items(&self) -> Vec<Item> {
        let conflicted: HashSet<&str> = self
            .book
            .conflicts
            .iter()
            .flat_map(|conflict| [conflict.path.as_str(), conflict.copy.as_str()])
            .collect();
        let mut items: Vec<Item> = self
            .book
            .manifest
            .iter()
            .filter(|(_, entry)| !entry.deleted)
            .map(|(path, entry)| {
                let there = self.remote.get(path) == Some(&entry.sha256);
                let state = if conflicted.contains(path.as_str()) {
                    ItemState::Conflict
                } else if entry.dir {
                    ItemState::Synced
                } else if self.sending.contains_key(path) && !there {
                    ItemState::Sending
                } else if !there {
                    // "On the phone too" only once the phone's own manifest
                    // lists this exact content: online, not sending, and not
                    // there yet is waiting (queued, paused or declined), not
                    // synced (S2-5).
                    ItemState::Waiting
                } else {
                    ItemState::Synced
                };
                Item {
                    dir: entry.dir,
                    path: path.clone(),
                    size: entry.size,
                    mtime: entry.mtime,
                    from: self.book.from.get(path).copied().unwrap_or(
                        if entry.vv.desk >= entry.vv.phone {
                            Side::Desk
                        } else {
                            Side::Phone
                        },
                    ),
                    state,
                }
            })
            .collect();
        items.extend(
            self.receiving
                .iter()
                .filter(|(path, _)| self.book.manifest.get(*path).is_none_or(|e| e.deleted))
                .map(|(path, entry)| Item {
                    dir: false,
                    path: path.clone(),
                    size: entry.size,
                    mtime: entry.mtime,
                    from: Side::Phone,
                    state: ItemState::Receiving,
                }),
        );
        items.sort_by_key(|item| std::cmp::Reverse(item.mtime));
        items
    }
}

impl Worker {
    pub(super) fn publish(&self) {
        if let Ok(mut shown) = SHOWN.lock() {
            shown.pairs = self
                .pairs
                .iter()
                .map(|live| live.view(self.ready.contains(&live.pair.device)))
                .collect();
            // Asked of a phone, waiting for it to choose its folder.
            shown
                .pairs
                .extend(self.asked.iter().map(|((device, id), (root, bucket, _))| {
                    PairView {
                        pair: Pair {
                            id: id.clone(),
                            name: root
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("Folder")
                                .to_string(),
                            root: root.clone(),
                            device: device.clone(),
                            include_hidden: false,
                            ignore: Vec::new(),
                            bucket: *bucket,
                        },
                        phase: Phase::Offered,
                        files: 0,
                        last_sync: None,
                        conflicts: Vec::new(),
                        items: Vec::new(),
                        bytes: 0,
                        skipped: Vec::new(),
                    }
                }));
            shown.dropped = self.dropped;
            shown.offers = self
                .offered
                .iter()
                .map(|(offer, _)| offer.clone())
                .collect();
        }
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn save_pairs(&self) {
        let pairs = Pairs {
            version: SCHEMA,
            pairs: self.pairs.iter().map(|live| live.pair.clone()).collect(),
            dropped: self.dropped,
        };
        save(&self.dir, PAIRS_FILE, PAIRS_AAD, &pairs);
    }

    pub(super) fn save_book(&mut self, index: usize) {
        let live = &mut self.pairs[index];
        live.book.version = SCHEMA;
        save(
            &self.dir,
            &book_file(&live.pair.device, &live.pair.id),
            BOOK_AAD,
            &live.book,
        );
    }

    /// A pair, by its phone and id (each phone's Bucket is `bucket`).
    pub(super) fn index_of(&self, device: &str, id: &str) -> Option<usize> {
        self.pairs
            .iter()
            .position(|live| live.pair.device == device && live.pair.id == id)
    }

    pub(super) fn handle(&mut self, message: Message) {
        match message {
            Message::Link(device, ready) => {
                if ready {
                    self.ready.insert(device.clone());
                    let ids: Vec<String> = self
                        .pairs
                        .iter()
                        .filter(|live| live.pair.device == device)
                        .map(|live| live.pair.id.clone())
                        .collect();
                    send(&device, &folders_packet("pairs", json!({"ids": ids})));
                    for live in self
                        .pairs
                        .iter_mut()
                        .filter(|live| live.pair.device == device)
                    {
                        // Scan now; the manifest follows the scan.
                        live.dirty = Some(
                            Instant::now()
                                .checked_sub(SETTLE)
                                .unwrap_or_else(Instant::now),
                        );
                        live.scanned = None;
                    }
                } else {
                    self.ready.remove(&device);
                    for live in self
                        .pairs
                        .iter_mut()
                        .filter(|live| live.pair.device == device)
                    {
                        live.phase = Phase::Waiting;
                        live.monitors.clear();
                        live.incoming.clear();
                        live.outstanding.clear();
                        live.to_request.clear();
                        live.receiving.clear();
                        live.sending.clear();
                        live.send_queue.clear();
                        live.flight.clear();
                    }
                    if let Ok(mut wanted) = WANTED.lock() {
                        wanted.retain(|(owner, _, _), _| *owner != device);
                    }
                }
                self.publish();
            }
            Message::Forget(device) => {
                // Syncing stops and what was requested is dropped.
                self.handle(Message::Link(device.clone(), false));
                let books: Vec<String> = self
                    .pairs
                    .iter()
                    .filter(|live| live.pair.device == device)
                    .map(|live| book_file(&device, &live.pair.id))
                    .collect();
                self.pairs.retain(|live| live.pair.device != device);
                for book in books {
                    let _ = fs::remove_file(self.dir.join(book));
                }
                self.offered.retain(|(offer, _)| offer.device != device);
                self.asked.retain(|(owner, _), _| *owner != device);
                self.save_pairs();
                self.publish();
            }
            Message::Packet(device, packet) => self.on_packet(&device, &packet),
            Message::Arrived {
                device,
                folder,
                stash,
                sha256,
                size,
            } => {
                self.place(&device, &folder, &stash, &sha256, size);
                let _ = fs::remove_file(&stash);
            }
            Message::Accept { device, id, root } => {
                let Some(position) = self
                    .offered
                    .iter()
                    .position(|(offer, _)| offer.id == id && offer.device == device)
                else {
                    return;
                };
                let (offer, _) = self.offered.remove(position);
                // The phone's Bucket needs no folder choice: it has its place.
                let root = if offer.bucket {
                    match self.make_bucket_root(&device) {
                        Some(root) => root,
                        None => return,
                    }
                } else {
                    root
                };
                if self.index_of(&device, &id).is_none() {
                    self.pairs.push(Live::new(
                        Pair {
                            id: id.clone(),
                            name: if offer.bucket {
                                "DeskLink Bucket".into()
                            } else {
                                offer.name
                            },
                            root,
                            device: device.clone(),
                            include_hidden: false,
                            ignore: Vec::new(),
                            bucket: offer.bucket,
                        },
                        Book::default(),
                    ));
                    self.save_pairs();
                }
                send(&device, &folders_packet("accept", json!({"id": id})));
                if let Some(index) = self.index_of(&device, &id) {
                    self.pairs[index].dirty = Some(
                        Instant::now()
                            .checked_sub(SETTLE)
                            .unwrap_or_else(Instant::now),
                    );
                }
                self.publish();
            }
            Message::Decline { device, id } => {
                self.offered
                    .retain(|(offer, _)| !(offer.id == id && offer.device == device));
                send(&device, &folders_packet("decline", json!({"id": id})));
                self.publish();
            }
            Message::Remove { device, id } => {
                if let Some(index) = self.index_of(&device, &id) {
                    let live = self.pairs.remove(index);
                    send(&device, &folders_packet("remove", json!({"id": id})));
                    let _ = fs::remove_file(self.dir.join(book_file(&device, &id)));
                    if live.pair.bucket {
                        bookmark(&live.pair.root, false);
                    }
                    self.save_pairs();
                }
                if let Some((root, bucket, _)) = self.asked.remove(&(device.clone(), id.clone())) {
                    send(&device, &folders_packet("remove", json!({"id": id})));
                    if bucket {
                        bookmark(&root, false);
                    }
                }
                self.publish();
            }
            Message::Resolve {
                device,
                pair,
                path,
                how,
            } => self.resolve(&device, &pair, &path, how),
            Message::ConfirmEmpty { device, id } => {
                if let Some(index) = self.index_of(&device, &id) {
                    let live = &mut self.pairs[index];
                    live.allow_empty = true;
                    live.dirty = Some(
                        Instant::now()
                            .checked_sub(SETTLE)
                            .unwrap_or_else(Instant::now),
                    );
                }
            }
            Message::TurnOnBucket { device } => self.turn_on_bucket(&device),
            Message::Import {
                device,
                sources,
                into,
            } => self.import(&device, sources, &into),
            Message::ImportBytes {
                device,
                name,
                bytes,
                into,
            } => self.import_bytes(&device, &name, bytes, &into),
            Message::Touched(device) => self.touched(&device),
            Message::SendFailed { device, path } => {
                if let Some(index) = self.index_of(&device, BUCKET_ID) {
                    self.pairs[index].flight.remove(&path);
                    self.pump_sends(index, &[]);
                }
            }
            Message::NewFolder { device, parent } => self.new_folder(&device, &parent),
            Message::MoveItems {
                device,
                paths,
                dest,
            } => self.move_items(&device, paths, &dest),
            Message::RemoveItems { device, paths } => self.remove_items(&device, paths),
        }
    }

    pub(super) fn tick(&mut self) {
        let dirty: Vec<String> = self.dirty.borrow_mut().drain().collect();
        for id in dirty {
            if let Some(live) = self.pairs.iter_mut().find(|live| live.pair.id == id) {
                live.dirty.get_or_insert_with(Instant::now);
            }
        }
        // Transfers that ended free their slots (F8c: 2 in flight).
        let ended = crate::device_links::webrtc::transfer_board::take_synced_ended();
        for index in 0..self.pairs.len() {
            let device = self.pairs[index].pair.device.clone();
            let sent: Vec<String> = ended
                .iter()
                .filter(|end| end.device == device && !end.incoming)
                .map(|end| end.name.clone())
                .collect();
            let failed: Vec<&str> = ended
                .iter()
                .filter(|end| end.device == device && end.incoming && !end.ok)
                .map(|end| end.name.as_str())
                .collect();
            let live = &mut self.pairs[index];
            if !failed.is_empty() {
                live.outstanding
                    .retain(|path| !failed.contains(&path.as_str()));
            }
            if !sent.is_empty() || !live.send_queue.is_empty() {
                self.pump_sends(index, &sent);
            }
            if !failed.is_empty() {
                self.request_more(index);
            }
        }
        let before = self.offered.len() + self.asked.len();
        self.offered.retain(|(_, at)| at.elapsed() < OFFER_LAPSES);
        self.asked
            .retain(|_, (_, _, at)| at.elapsed() < OFFER_LAPSES);
        let mut changed = before != self.offered.len() + self.asked.len();
        for index in 0..self.pairs.len() {
            let live = &self.pairs[index];
            if !self.ready.contains(&live.pair.device) || !live.on_phone {
                continue;
            }
            let due = live.dirty.is_some_and(|at| at.elapsed() >= SETTLE)
                || live.scanned.is_none_or(|at| at.elapsed() >= RESCAN_EVERY);
            if due {
                self.rescan(index);
                changed = true;
            }
        }
        if changed {
            self.publish();
        }
    }

    /// Scans a pair, re-watches its directories, and sends the manifest
    /// when something changed (or it never went out on this link).
    pub(super) fn rescan(&mut self, index: usize) {
        let first = self.pairs[index].scanned.is_none();
        let live = &mut self.pairs[index];
        // Spent by this scan, whatever it finds: a later empty scan (an
        // unmounted drive) is held again.
        let allow_empty = std::mem::take(&mut live.allow_empty);
        live.dirty = None;
        live.scanned = Some(Instant::now());
        live.phase = Phase::Scanning;
        let pair = live.pair.clone();
        let scanned = scan(&pair);
        let (mut found, directories, too_many, skipped) = match scanned {
            Ok(scanned) => scanned,
            Err(problem) => {
                live.phase = Phase::Problem(problem);
                return;
            }
        };
        live.skipped = skipped;
        let had_files = live.book.manifest.values().any(|entry| !entry.deleted);
        let stamp = RootStamp::of(&pair.root);
        if found.is_empty()
            && had_files
            && !allow_empty
            && !emptied_on_purpose(live.root_stamp, stamp)
        {
            // An unmounted drive looks exactly like this: deleting every
            // file on the phone too would be the worst outcome.
            live.phase = Phase::EmptyHeld;
            return;
        }
        if stamp.is_some() {
            live.root_stamp = stamp;
        }
        // A file still being written waits until it has held for 2 s.
        if hold_unstable(&mut found, &live.book.manifest, now_ms(), stable_ms()) {
            live.dirty = Some(Instant::now());
        }
        let root = pair.root.clone();
        keep_existing(&mut found, &live.book.manifest, &root);
        let changed = apply_scan(
            Side::Desk,
            &mut live.book.manifest,
            &found,
            &mut |path| sha256_of(&root.join(path)),
            now_ms(),
        );
        // New here: from this computer. Forgotten files go.
        for path in found.keys() {
            if !live.book.from.contains_key(path) {
                live.book.from.insert(path.clone(), Side::Desk);
            }
        }
        let manifest = &live.book.manifest;
        live.book
            .from
            .retain(|path, _| manifest.get(path).is_some_and(|entry| !entry.deleted));
        // Watch what exists now.
        let wanted: HashSet<PathBuf> = directories.into_iter().collect();
        live.monitors.retain(|dir, _| wanted.contains(dir));
        for dir in wanted {
            if live.monitors.contains_key(&dir) || live.monitors.len() >= MAX_WATCHED_DIRS {
                continue;
            }
            let file = gtk::gio::File::for_path(&dir);
            if let Ok(monitor) = gtk::prelude::FileExt::monitor_directory(
                &file,
                gtk::gio::FileMonitorFlags::WATCH_MOVES,
                gtk::gio::Cancellable::NONE,
            ) {
                let (dirty, id) = (std::rc::Rc::clone(&self.dirty), pair.id.clone());
                gtk::prelude::FileMonitorExt::connect_changed(&monitor, move |_, _, _, _| {
                    live_dirty(&dirty, &id);
                });
                live.monitors.insert(dir, monitor);
            }
        }
        live.phase = if too_many {
            Phase::TooMany
        } else if live.outstanding.is_empty() && live.to_request.is_empty() {
            Phase::UpToDate
        } else {
            Phase::Syncing(live.outstanding.len() + live.to_request.len())
        };
        if changed {
            self.save_book(index);
        }
        if changed || first {
            self.send_manifest(index);
        }
    }
}

/// Work that copies or snapshots whole files runs on its own thread, so the
/// folder worker keeps answering packets and ticks (and its queue of 512
/// cannot fill while a gigabyte is copied).
type Job = Box<dyn FnOnce() + Send>;
/// Jobs waiting for a lane; past it the caller is told "busy".
pub(super) const LANE_QUEUE: usize = 8;
pub(super) static COPY_LANE: OnceLock<SyncSender<Job>> = OnceLock::new();
pub(super) static SEND_LANE: OnceLock<SyncSender<Job>> = OnceLock::new();

/// Runs `job` on `lane`'s thread (tests: at once); false when the lane is
/// full.
pub(super) fn offload(lane: &'static OnceLock<SyncSender<Job>>, name: &str, job: Job) -> bool {
    #[cfg(test)]
    {
        let _ = (lane, name);
        job();
        true
    }
    #[cfg(not(test))]
    {
        let sender = lane.get_or_init(|| {
            let (sender, jobs) = mpsc::sync_channel::<Job>(LANE_QUEUE);
            let _ = std::thread::Builder::new()
                .name(name.to_string())
                .spawn(move || {
                    for job in jobs {
                        crate::guard("folder copy", (), job);
                    }
                });
            sender
        });
        sender.try_send(job).is_ok()
    }
}

/// A send that never showed on the board lapses after this.
pub(super) const FLIGHT_LAPSES: Duration = Duration::from_secs(1800);

/// F8c: the files to start sending now: the slots of `ended` are freed,
/// then the queue fills up to [`MAX_IN_FLIGHT`] in flight.
pub(super) fn due_sends(
    queue: &mut VecDeque<String>,
    flight: &mut HashMap<String, Instant>,
    ended: &[String],
    now: Instant,
) -> Vec<String> {
    for path in ended {
        flight.remove(path);
    }
    flight.retain(|_, at| now.saturating_duration_since(*at) < FLIGHT_LAPSES);
    let mut due = Vec::new();
    while flight.len() < MAX_IN_FLIGHT {
        let Some(path) = queue.pop_front() else {
            break;
        };
        if flight.insert(path.clone(), now).is_none() {
            due.push(path);
        }
    }
    due
}

pub(super) fn live_dirty(dirty: &std::rc::Rc<std::cell::RefCell<HashSet<String>>>, id: &str) {
    let mut dirty = dirty.borrow_mut();
    if dirty.len() < 1024 {
        dirty.insert(id.to_string());
    }
}
