//! The worker's sync engine (`impl Worker`, see `worker`): packets from the
//! phone, manifests, applying a plan, requests, placing received files, sends
//! and conflict resolution.
//!
//! Sibling edges: `worker`, `model`, `state`, `wire`, `disk`, `scan`, `store`,
//! `import`, `report`. Outside: `connection_log`, `desk_features`.

use super::*;

impl Worker {
    /// Starts the sends that may start now (see [`due_sends`]).
    pub(super) fn pump_sends(&mut self, index: usize, ended: &[String]) {
        let live = &mut self.pairs[index];
        let due = due_sends(
            &mut live.send_queue,
            &mut live.flight,
            ended,
            Instant::now(),
        );
        let (device, id, root) = (
            live.pair.device.clone(),
            live.pair.id.clone(),
            live.pair.root.clone(),
        );
        let mut due = due.into_iter();
        while let Some(path) = due.next() {
            let Some(entry) = live.book.manifest.get(&path).filter(|entry| !entry.deleted) else {
                live.flight.remove(&path);
                continue;
            };
            let Ok(source) = read_path(&root, &path) else {
                live.flight.remove(&path);
                continue;
            };
            if !unchanged(&source, Some(entry)) {
                // Changed since it was asked for: scanned again; the new
                // version is asked for next.
                live.flight.remove(&path);
                live.dirty = Some(Instant::now());
                continue;
            }
            let folder = FolderRef {
                folder: id.clone(),
                relpath: path.clone(),
                mtime: Some(entry.mtime),
            };
            // Not on this worker: the transport may wait for the peer.
            let (owner, sent) = (device.clone(), path.clone());
            let content = entry.sha256.clone();
            let job = move || {
                if let Err(error) = crate::device_links::desk_features::send_folder_file(
                    &owner, &source, folder, &content,
                ) {
                    log_limited("send", &format!("A synced file was not sent: {error}"));
                    send_message(Message::SendFailed {
                        device: owner,
                        path: sent,
                    });
                }
            };
            if !offload(&SEND_LANE, "desklink-bucket-send", Box::new(job)) {
                // Busy: this and the rest go back to the front of the queue,
                // in order; the next tick retries.
                for waiting in std::iter::once(path).chain(due).rev() {
                    live.flight.remove(&waiting);
                    live.send_queue.push_front(waiting);
                }
                break;
            }
        }
    }

    /// Something was added: scan the Bucket soon.
    pub(super) fn touched(&mut self, device: &str) {
        if let Some(index) = self.index_of(device, BUCKET_ID) {
            self.pairs[index].dirty = Some(
                Instant::now()
                    .checked_sub(SETTLE)
                    .unwrap_or_else(Instant::now),
            );
        }
    }

    pub(super) fn on_packet(&mut self, device: &str, packet: &NetworkPacket) {
        let id = packet
            .get_str("id")
            .filter(|id| valid_pair_id(id))
            .map(str::to_string);
        match (packet.get_str("op"), id) {
            (Some("pairs"), _) => {
                let listed: HashSet<String> = packet
                    .body
                    .get("ids")
                    .and_then(Value::as_array)
                    .map(|ids| {
                        ids.iter()
                            .take(64)
                            .filter_map(|id| id.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                for live in self
                    .pairs
                    .iter_mut()
                    .filter(|live| live.pair.device == device)
                {
                    live.on_phone = listed.contains(&live.pair.id);
                    if !live.on_phone {
                        live.phase = Phase::NotOnPhone;
                    }
                }
                self.publish();
            }
            (Some("offer"), Some(id)) => {
                let name: String = packet
                    .get_str("name")
                    .unwrap_or("Folder")
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(MAX_NAME_CHARS)
                    .collect();
                let known = self.index_of(device, &id).is_some()
                    || self
                        .offered
                        .iter()
                        .any(|(offer, _)| offer.id == id && offer.device == device);
                let room = self.offered.len() < MAX_PENDING_OFFERS
                    && self
                        .pairs
                        .iter()
                        .filter(|live| live.pair.device == device)
                        .count()
                        < MAX_PAIRS_PER_DEVICE;
                if known {
                    return;
                }
                if !room {
                    send(device, &folders_packet("decline", json!({"id": id})));
                    return;
                }
                let bucket = id == BUCKET_ID && packet.get_bool("bucket").unwrap_or(false);
                if !bucket {
                    // The Bucket is the only pair now.
                    send(device, &folders_packet("decline", json!({"id": id})));
                    return;
                }
                self.offered.push((
                    Offer {
                        device: device.to_string(),
                        id,
                        name,
                        bucket,
                    },
                    Instant::now(),
                ));
                self.publish();
            }
            (Some("accept"), Some(id)) => {
                let Some((root, bucket, _)) = self.asked.remove(&(device.to_string(), id.clone()))
                else {
                    return;
                };
                let name: String = root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("Folder")
                    .chars()
                    .take(MAX_NAME_CHARS)
                    .collect();
                let mut live = Live::new(
                    Pair {
                        id,
                        name,
                        root,
                        device: device.to_string(),
                        include_hidden: false,
                        ignore: Vec::new(),
                        bucket,
                    },
                    Book::default(),
                );
                live.dirty = Some(
                    Instant::now()
                        .checked_sub(SETTLE)
                        .unwrap_or_else(Instant::now),
                );
                self.pairs.push(live);
                self.save_pairs();
                self.publish();
            }
            (Some("decline"), Some(id)) => {
                if let Some((root, bucket, _)) = self.asked.remove(&(device.to_string(), id)) {
                    if bucket {
                        bookmark(&root, false);
                    }
                    self.publish();
                }
            }
            (Some("remove"), Some(id)) => {
                if let Some(index) = self.index_of(device, &id) {
                    let live = self.pairs.remove(index);
                    let _ = fs::remove_file(self.dir.join(book_file(device, &id)));
                    if live.pair.bucket {
                        bookmark(&live.pair.root, false);
                    }
                    self.save_pairs();
                }
                self.offered
                    .retain(|(offer, _)| !(offer.id == id && offer.device == device));
                self.asked.remove(&(device.to_string(), id));
                self.publish();
            }
            (Some("manifest"), Some(id)) => self.on_manifest(device, &id, packet),
            (Some("request"), Some(id)) => self.on_request(device, &id, packet),
            _ => {}
        }
    }

    pub(super) fn on_manifest(&mut self, device: &str, id: &str, packet: &NetworkPacket) {
        let Some(index) = self.index_of(device, id) else {
            return;
        };
        let part = packet.get_i64("part").unwrap_or(0);
        let parts = packet.get_i64("parts").unwrap_or(0);
        if !(1..=MAX_PARTS as i64).contains(&parts) || !(1..=parts).contains(&part) {
            return;
        }
        let live = &mut self.pairs[index];
        live.on_phone = true;
        let held: usize = live.incoming.values().map(Vec::len).sum();
        if part == 1 {
            live.incoming.clear();
        }
        if held >= MAX_FILES * 2 {
            live.incoming.clear();
            return;
        }
        let entries = manifest_entries(packet, &live.pair);
        live.incoming.insert(part as u32, entries);
        if live.incoming.len() as i64 != parts {
            return;
        }
        let mut remote: Manifest = std::mem::take(&mut live.incoming)
            .into_values()
            .flatten()
            .take(MAX_FILES)
            .collect();
        if live.pair.bucket {
            let left_out = left_out_of_remote(&mut remote, &live.book.manifest);
            for path in &left_out {
                debug_log!("[DeskLink] Bucket: {path} is not synced (differs only in case)");
            }
            live.remote_clashes = left_out
                .into_iter()
                .map(|path| format!("{path} ({CASE_CLASH})"))
                .collect();
        }
        self.apply_plan(index, &remote);
    }

    /// P3 for one pair: carry out the plan against the other's manifest.
    fn apply_plan(&mut self, index: usize, remote: &Manifest) {
        let phone = crate::device_links::connection_log::name_of(&self.pairs[index].pair.device);
        let steps = plan(Side::Desk, &self.pairs[index].book.manifest, remote, &phone);
        let root = self.pairs[index].pair.root.clone();
        let pair_id = self.pairs[index].pair.id.clone();
        let device = self.pairs[index].pair.device.clone();
        let mut downloads = Vec::new();
        let mut restored: Vec<String> = Vec::new();
        let mut rescan = false;
        let mut kept = false;
        {
            let live = &mut self.pairs[index];
            let manifest = &mut live.book.manifest;
            // A folder the other side removed, with only files that go too:
            // one trash item.
            let removals: HashMap<String, Entry> = steps
                .iter()
                .filter_map(|step| match step {
                    Step::Delete { path, entry } => Some((path.clone(), entry.clone())),
                    _ => None,
                })
                .collect();
            let mut handled: HashSet<String> = HashSet::new();
            let mut folders: Vec<&String> = removals
                .iter()
                .filter(|(_, entry)| entry.dir)
                .map(|(path, _)| path)
                .collect();
            folders.sort_by_key(|path| path.len());
            for folder in folders {
                if handled.contains(folder) {
                    continue;
                }
                let below_prefix = format!("{folder}/");
                let below: Vec<(String, Entry)> = manifest
                    .iter()
                    .filter(|(path, entry)| path.starts_with(&below_prefix) && !entry.deleted)
                    .map(|(path, entry)| (path.clone(), entry.clone()))
                    .collect();
                let target = root.join(folder);
                let tidy = !below.is_empty()
                    && target.is_dir()
                    && below.iter().all(|(path, entry)| {
                        removals.contains_key(path) && unchanged(&root.join(path), Some(entry))
                    })
                    && count_below(&target, MAX_BUCKET_ITEMS) == below.len();
                if tidy && trash(&target) {
                    handled.insert(folder.clone());
                    manifest.insert(folder.clone(), removals[folder].clone());
                    for (path, _) in below {
                        manifest.insert(path.clone(), removals[&path].clone());
                        handled.insert(path);
                    }
                }
            }
            let mut folder_deletes: Vec<(String, Entry)> = Vec::new();
            for step in steps {
                if matches!(&step, Step::Delete { path, .. } if handled.contains(path)) {
                    continue;
                }
                match step {
                    Step::Take { path, entry } => {
                        manifest.insert(path, entry);
                    }
                    Step::Bump { path, vv } => {
                        if let Some(entry) = manifest.get_mut(&path) {
                            entry.vv = vv;
                        }
                    }
                    Step::Delete { path, entry } if entry.dir => {
                        folder_deletes.push((path, entry));
                    }
                    Step::Delete { path, entry } => match read_path(&root, &path) {
                        Ok(target) if unchanged(&target, manifest.get(&path)) => {
                            if !target.exists() || trash(&target) {
                                manifest.insert(path, entry);
                            } else {
                                // Kept: the next round asks again, the user
                                // is told meanwhile.
                                kept = true;
                            }
                        }
                        _ => rescan = true,
                    },
                    Step::Move {
                        from,
                        to,
                        entry,
                        tombstone,
                    } => {
                        let moved = read_path(&root, &from)
                            .and_then(|source| Ok((source, target_path(&root, &to)?)))
                            .ok()
                            .filter(|(source, target)| {
                                unchanged(source, manifest.get(&from))
                                    && unchanged(target, manifest.get(&to))
                            })
                            .is_some_and(|(source, target)| fs::rename(source, target).is_ok());
                        if moved {
                            manifest.insert(from, tombstone);
                            live.book.from.insert(to.clone(), Side::Phone);
                            manifest.insert(to, entry);
                        } else {
                            rescan = true;
                        }
                    }
                    Step::Copy { from, to, entry } => {
                        let copied = read_path(&root, &from)
                            .and_then(|source| Ok((source, target_path(&root, &to)?)))
                            .ok()
                            .filter(|(source, target)| {
                                unchanged(source, manifest.get(&from))
                                    && unchanged(target, manifest.get(&to))
                            })
                            .is_some_and(|(source, target)| {
                                let staged = target.with_file_name(format!(
                                    ".desklink-{}.tmp",
                                    uuid::Uuid::new_v4().simple()
                                ));
                                fs::copy(&source, &staged).is_ok()
                                    && put_in_place(&staged, &target, entry.mtime).is_ok()
                            });
                        if copied {
                            live.book.from.insert(to.clone(), Side::Phone);
                            manifest.insert(to, entry);
                        } else {
                            downloads.push((to, entry));
                        }
                    }
                    Step::Download { path, entry } if entry.dir => match make_dir(&root, &path) {
                        Ok(_) => {
                            manifest.insert(path, entry);
                        }
                        Err(_) => rescan = true,
                    },
                    Step::Download { path, entry } => {
                        // A file removed here that the other side changed
                        // comes back; the person is told (C10).
                        if live.pair.bucket
                            && manifest.get(&path).is_some_and(|ours| ours.deleted)
                            && !entry.deleted
                        {
                            restored.push(path.clone());
                        }
                        downloads.push((path, entry));
                    }
                    Step::Conflict { path, copy } => {
                        let conflicts = &mut live.book.conflicts;
                        if !conflicts.iter().any(|known| known.copy == copy)
                            && conflicts.len() < MAX_CONFLICTS
                        {
                            conflicts.push(Conflict {
                                path,
                                copy,
                                at: now_ms(),
                            });
                        }
                    }
                    // The desk never renames its own version (P3).
                    Step::Rename { .. } => {}
                }
            }
            // Folders go last, the deepest first, and only when empty: one
            // that still holds something (a change not synced yet) stays, and
            // the next scan tells the other side it is still here.
            folder_deletes.sort_by_key(|(path, _)| std::cmp::Reverse(path.len()));
            for (path, entry) in folder_deletes {
                let target = root.join(&path);
                if !target.exists() || fs::remove_dir(&target).is_ok() {
                    manifest.insert(path, entry);
                } else if let Some(ours) = manifest.get(&path).cloned() {
                    // Kept: our version now dominates the removal, so the
                    // phone gets the folder back.
                    manifest.insert(
                        path,
                        Entry {
                            vv: ours.vv.max(entry.vv).bumped(Side::Desk),
                            ..ours
                        },
                    );
                }
            }
            if !restored.is_empty() {
                restored.truncate(MAX_SKIPPED_SHOWN);
                tell(&Report {
                    restored: std::mem::take(&mut restored),
                    ..Report::default()
                });
            }
            if let Ok(mut wanted) = WANTED.lock() {
                for (path, entry) in &downloads {
                    if wanted.len() < MAX_OUTSTANDING * MAX_PAIRS_PER_DEVICE * 4 {
                        wanted.insert(
                            (device.clone(), pair_id.clone(), path.clone()),
                            entry.clone(),
                        );
                    }
                }
            }
            live.to_request = downloads.iter().map(|(path, _)| path.clone()).collect();
            live.receiving = downloads.iter().cloned().collect();
            live.outstanding.clear();
            live.remote = remote
                .iter()
                .filter(|(_, entry)| !entry.deleted)
                .map(|(path, entry)| (path.clone(), entry.sha256.clone()))
                .collect();
            live.sending
                .retain(|path, sha| live.remote.get(path) != Some(sha));
            if rescan {
                live.dirty = Some(Instant::now());
            }
        }
        self.request_more(index);
        let live = &mut self.pairs[index];
        live.phase = if kept {
            Phase::Problem(TRASH_MISSING.into())
        } else if live.to_request.is_empty() && live.outstanding.is_empty() {
            live.book.last_sync = Some(now_ms());
            Phase::UpToDate
        } else {
            Phase::Syncing(live.to_request.len() + live.outstanding.len())
        };
        self.save_book(index);
        self.publish();
    }

    /// Asks for the next paths, up to [`MAX_OUTSTANDING`] at a time.
    pub(super) fn request_more(&mut self, index: usize) {
        let live = &mut self.pairs[index];
        let mut batch = Vec::new();
        // And one data-channel message: paths may be 1 KiB each.
        let mut bytes = 0;
        while live.outstanding.len() < MAX_IN_FLIGHT
            && batch.len() < REQUEST_PATHS
            && bytes < MAX_PACKET_BYTES / 2
        {
            let Some(path) = live.to_request.pop_front() else {
                break;
            };
            bytes += path.len() * 2 + 3;
            live.outstanding.push_back(path.clone());
            batch.push(path);
        }
        if !batch.is_empty() {
            send(
                &live.pair.device,
                &folders_packet("request", json!({"id": live.pair.id, "paths": batch})),
            );
        }
    }

    /// P4: the phone asks for files: each sent if it still matches.
    fn on_request(&mut self, device: &str, id: &str, packet: &NetworkPacket) {
        let Some(index) = self.index_of(device, id) else {
            return;
        };
        let live = &mut self.pairs[index];
        let paths: Vec<String> = packet
            .body
            .get("paths")
            .and_then(Value::as_array)
            .map(|paths| {
                paths
                    .iter()
                    .take(REQUEST_PATHS)
                    .filter_map(|path| path.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        for path in paths {
            let Some(entry) = live
                .book
                .manifest
                .get(&path)
                .filter(|entry| !entry.deleted && !entry.dir)
            else {
                continue;
            };
            let Ok(source) = read_path(&live.pair.root, &path) else {
                continue;
            };
            if !unchanged(&source, Some(entry)) {
                live_dirty(&self.dirty, id);
                continue;
            }
            if live.sending.len() < MAX_OUTSTANDING {
                live.sending.insert(path.clone(), entry.sha256.clone());
            }
            if live.send_queue.len() < MAX_OUTSTANDING && !live.flight.contains_key(&path) {
                live.send_queue.push_back(path);
            }
        }
        self.pump_sends(index, &[]);
    }

    /// A requested file arrived: checked, put in place, recorded.
    pub(super) fn place(
        &mut self,
        device: &str,
        folder: &FolderRef,
        stash: &Path,
        sha256: &str,
        size: u64,
    ) {
        let key = (
            device.to_string(),
            folder.folder.clone(),
            folder.relpath.clone(),
        );
        // Only the request this file answers: a stale arrival (the file
        // changed on the phone meanwhile and was asked for again) must leave
        // the newer request, or its offer is refused and the Bucket stays
        // "Syncing" until the next manifest.
        let Some(entry) = take_wanted_matching(&key, sha256, size) else {
            return;
        };
        let Some(index) = self.index_of(device, &folder.folder) else {
            return;
        };
        let live = &mut self.pairs[index];
        live.outstanding.retain(|path| *path != folder.relpath);
        let placed = target_path(&live.pair.root, &folder.relpath).and_then(|target| {
            if !unchanged(&target, live.book.manifest.get(&folder.relpath)) {
                return Err("changed here meanwhile".into());
            }
            put_in_place(stash, &target, entry.mtime)?;
            let metadata = fs::metadata(&target).map_err(|_| "vanished".to_string())?;
            Ok(Stat {
                size: metadata.len(),
                mtime: mtime_ms(&metadata),
                dir: false,
            })
        });
        match placed {
            Ok(stat) => {
                live.receiving.remove(&folder.relpath);
                live.book.from.insert(folder.relpath.clone(), Side::Phone);
                live.remote
                    .insert(folder.relpath.clone(), entry.sha256.clone());
                live.book.manifest.insert(
                    folder.relpath.clone(),
                    Entry {
                        size: stat.size,
                        mtime: stat.mtime,
                        ..entry
                    },
                );
            }
            Err(_) => live.dirty = Some(Instant::now()),
        }
        self.request_more(index);
        let live = &mut self.pairs[index];
        if live.outstanding.is_empty() && live.to_request.is_empty() {
            live.phase = Phase::UpToDate;
            live.book.last_sync = Some(now_ms());
            self.save_book(index);
            self.send_manifest(index);
        } else {
            live.phase = Phase::Syncing(live.outstanding.len() + live.to_request.len());
        }
        self.publish();
    }

    pub(super) fn send_manifest(&self, index: usize) {
        let live = &self.pairs[index];
        if !self.ready.contains(&live.pair.device) {
            return;
        }
        for packet in manifest_packets(&live.pair.id, &live.book.manifest) {
            send(&live.pair.device, &packet);
        }
    }

    pub(super) fn resolve(&mut self, device: &str, pair: &str, path: &str, how: Resolve) {
        let Some(index) = self.index_of(device, pair) else {
            return;
        };
        let live = &mut self.pairs[index];
        let Some(position) = live
            .book
            .conflicts
            .iter()
            .position(|known| known.path == path)
        else {
            return;
        };
        let conflict = live.book.conflicts.remove(position);
        if let Ok(mut set) = refused_trash().lock() {
            set.remove(&(device.to_string(), conflict.path.clone()));
        }
        let root = live.pair.root.clone();
        match how {
            Resolve::Both => {}
            Resolve::Desk => {
                if let Ok(copy) = read_path(&root, &conflict.copy) {
                    // Without a trash the phone's copy stays (a file is never
                    // deleted for good because someone chose a side): said, so
                    // the person does not think it is gone.
                    if copy.exists() && !trash(&copy) {
                        tell(&Report {
                            refused: Some(
                                "There is no trash to put the phone's copy in, so it was kept as a file here."
                                    .to_string(),
                            ),
                            ..Report::default()
                        });
                    }
                }
            }
            Resolve::Phone | Resolve::PhoneAnyway => {
                if let (Ok(copy), Ok(original)) = (
                    read_path(&root, &conflict.copy),
                    read_path(&root, &conflict.path),
                ) {
                    // This computer's version goes to the trash first: the
                    // rename alone replaced it for good (S4). Without a
                    // trash it stays, and the conflict with it.
                    if original.exists() && !trash(&original) && how != Resolve::PhoneAnyway {
                        refused_trash()
                            .lock()
                            .map(|mut set| set.insert((device.to_string(), conflict.path.clone())))
                            .ok();
                        live.book.conflicts.insert(position, conflict);
                        tell(&Report {
                            refused: Some(
                                "There is no trash to put this computer's version in, so nothing was replaced. Choose Replace Anyway to replace it for good."
                                    .to_string(),
                            ),
                            ..Report::default()
                        });
                        self.publish();
                        return;
                    }
                    let _ = fs::rename(copy, original);
                }
            }
        }
        live.dirty = Some(Instant::now());
        self.save_book(index);
        self.publish();
    }
}
