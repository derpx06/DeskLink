//! The worker's Bucket operations (`impl Worker`, see `worker`): turning a
//! Bucket on, adding, moving and removing items, and new folders.
//!
//! Sibling edges: `worker`, `engine` (`pump_sends`), `import`, `disk`, `model`,
//! `report`, `scan`, `wire`. Outside: `connection_log`, `desk_features`,
//! `webrtc::transfer_board`.

use super::*;

impl Worker {
    /// Creates (or reuses) the folder for `device`'s Bucket and bookmarks
    /// it. `None` when it cannot be made.
    pub(super) fn make_bucket_root(&mut self, device: &str) -> Option<PathBuf> {
        let taken: Vec<PathBuf> = self
            .pairs
            .iter()
            .filter(|live| live.pair.device != device)
            .map(|live| live.pair.root.clone())
            .chain(
                self.asked
                    .iter()
                    .filter(|((owner, _), _)| owner != device)
                    .map(|(_, (root, _, _))| root.clone()),
            )
            .collect();
        let phone = crate::device_links::connection_log::name_of(device);
        let root = bucket_root(&home(), &phone, &taken);
        if fs::create_dir_all(&root).is_err() {
            log_limited("bucket-dir", "Could not create the Bucket folder");
            return None;
        }
        bookmark(&root, true);
        Some(root)
    }

    /// F8b: one tap. The folder is made here; the phone picks its side.
    pub(super) fn turn_on_bucket(&mut self, device: &str) {
        if self.index_of(device, BUCKET_ID).is_some()
            || self
                .asked
                .contains_key(&(device.to_string(), BUCKET_ID.to_string()))
        {
            return;
        }
        // The phone asked first: turning it on here is the answer.
        if self
            .offered
            .iter()
            .any(|(offer, _)| offer.device == device && offer.id == BUCKET_ID)
        {
            self.handle(Message::Accept {
                device: device.to_string(),
                id: BUCKET_ID.to_string(),
                root: PathBuf::new(),
            });
            return;
        }
        let Some(root) = self.make_bucket_root(device) else {
            return;
        };
        if self.dropped > 0 {
            self.dropped = 0;
            self.save_pairs();
        }
        send(
            device,
            &folders_packet(
                "offer",
                json!({"id": BUCKET_ID, "name": "DeskLink Bucket", "bucket": true}),
            ),
        );
        self.asked.insert(
            (device.to_string(), BUCKET_ID.to_string()),
            (root, true, Instant::now()),
        );
        self.publish();
    }

    /// The root things are added to: the Bucket's, while it is on (or
    /// waiting for the phone).
    fn bucket_dir(&self, device: &str) -> Option<PathBuf> {
        self.index_of(device, BUCKET_ID)
            .map(|index| self.pairs[index].pair.root.clone())
            .or_else(|| {
                self.asked
                    .get(&(device.to_string(), BUCKET_ID.to_string()))
                    .map(|(root, _, _)| root.clone())
            })
    }

    /// What `device`'s Bucket holds: bytes and items (files and folders).
    fn bucket_used(&self, device: &str) -> (u64, usize) {
        self.index_of(device, BUCKET_ID)
            .map(|index| {
                let live = self.pairs[index].book.manifest.values();
                live.filter(|entry| !entry.deleted)
                    .fold((0, 0), |(bytes, items), entry| {
                        (bytes + entry.size, items + 1)
                    })
            })
            .unwrap_or((0, 0))
    }

    pub(super) fn import(&mut self, device: &str, sources: Vec<PathBuf>, into: &str) {
        let Some(dir) = self.bucket_dir(device) else {
            return;
        };
        let mut report = Report::default();
        if !(into.is_empty() || bucket_path_ok(into, true))
            || make_dir(&dir, into).is_err() && !into.is_empty()
        {
            report.refused = Some("That folder isn't in the Bucket".into());
            tell(&report);
            return;
        }
        let adds = collect(&sources, &dir, into, self.bucket_used(device), &mut report);
        let owner = device.to_string();
        let job = move || {
            (report.added, report.failed) = copy_adds(&adds, &dir);
            tell(&report);
            send_message(Message::Touched(owner));
        };
        if !offload(&COPY_LANE, "desklink-bucket-copy", Box::new(job)) {
            tell(&Report {
                refused: Some("Still copying. Try again in a moment".into()),
                ..Report::default()
            });
        }
        #[cfg(test)]
        self.touched(device);
    }

    pub(super) fn import_bytes(&mut self, device: &str, name: &str, bytes: Vec<u8>, into: &str) {
        let Some(dir) = self.bucket_dir(device) else {
            return;
        };
        let (used_bytes, used_items) = self.bucket_used(device);
        let mut report = Report::default();
        if !name_ok(name) || name.contains('/') || !(into.is_empty() || bucket_path_ok(into, true))
        {
            report.refused = Some("That name cannot be used".into());
        } else if used_items >= MAX_BUCKET_ITEMS {
            report.over_items = 1;
        } else if used_bytes + bytes.len() as u64 > BUCKET_FULL_BYTES {
            report.full = true;
        }
        if report != Report::default() {
            tell(&report);
            return;
        }
        let Ok(folder) = make_dir(&dir, into).or_else(|_| {
            if into.is_empty() {
                Ok(dir.clone())
            } else {
                Err(())
            }
        }) else {
            return;
        };
        let (owner, name) = (device.to_string(), name.to_string());
        let job = move || {
            let target = unique_in(&folder, &name);
            let staged =
                target.with_file_name(format!(".desklink-{}.tmp", uuid::Uuid::new_v4().simple()));
            let written = fs::write(&staged, &bytes).is_ok()
                && put_in_place(&staged, &target, now_ms()).is_ok();
            if !written {
                let _ = fs::remove_file(&staged);
                log_limited("import", "Could not save a pasted item in the Bucket");
            }
            send_message(Message::Touched(owner));
        };
        if !offload(&COPY_LANE, "desklink-bucket-copy", Box::new(job)) {
            tell(&Report {
                refused: Some("Still copying. Try again in a moment".into()),
                ..Report::default()
            });
        }
        #[cfg(test)]
        self.touched(device);
    }

    /// F8c: "New Folder" in `parent`; not past the depth limit.
    pub(super) fn new_folder(&mut self, device: &str, parent: &str) {
        let Some(dir) = self.bucket_dir(device) else {
            return;
        };
        let depth = if parent.is_empty() {
            0
        } else {
            parent.split('/').count()
        };
        let mut report = Report::default();
        if depth >= MAX_BUCKET_DEPTH || !(parent.is_empty() || bucket_path_ok(parent, true)) {
            report.refused = Some(format!(
                "Folders can only go {MAX_BUCKET_DEPTH} levels deep in the Bucket"
            ));
        } else if self.bucket_used(device).1 >= MAX_BUCKET_ITEMS {
            report.over_items = 1;
        } else if let Ok(folder) = make_dir(&dir, parent) {
            let _ = fs::create_dir(unique_in(&folder, "New Folder"));
            self.touched(device);
        }
        tell(&report);
    }

    /// F8c: moves items into folder `dest` ("" the top); an item that
    /// would end up past the depth limit stays and is counted.
    pub(super) fn move_items(&mut self, device: &str, paths: Vec<String>, dest: &str) {
        let (Some(index), Some(root)) = (self.index_of(device, BUCKET_ID), self.bucket_dir(device))
        else {
            return;
        };
        let mut report = Report::default();
        if !(dest.is_empty() || bucket_path_ok(dest, true))
            || make_dir(&root, dest).is_err() && !dest.is_empty()
        {
            report.refused = Some("That folder isn't in the Bucket".into());
            tell(&report);
            return;
        }
        let dest_depth = if dest.is_empty() {
            0
        } else {
            dest.split('/').count()
        };
        let manifest = &self.pairs[index].book.manifest;
        let mut moved = false;
        for path in paths.iter().take(MAX_ADD_ITEMS) {
            let Some(entry) = manifest.get(path).filter(|entry| !entry.deleted) else {
                continue;
            };
            let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
            if parent == dest {
                continue;
            }
            if dest == path || dest.starts_with(&format!("{path}/")) {
                report.refused = Some("A folder can't go inside itself".into());
                continue;
            }
            if entry.dir {
                let below = format!("{path}/");
                let height = manifest
                    .iter()
                    .filter(|(other, found)| {
                        found.dir && !found.deleted && other.starts_with(&below)
                    })
                    .map(|(other, _)| levels(other) - levels(path))
                    .max()
                    .unwrap_or(0);
                if dest_depth + 1 + height > MAX_BUCKET_DEPTH {
                    report.not_moved += 1;
                    continue;
                }
            }
            let name = path.rsplit('/').next().unwrap_or(path);
            let target = unique_in(&root.join(dest), name);
            moved |= fs::rename(root.join(path), target).is_ok();
        }
        tell(&report);
        if moved {
            self.touched(device);
        }
    }

    /// F8c: removes items from both sides. Transfers of them are cancelled
    /// first; a folder goes to the trash as one item.
    pub(super) fn remove_items(&mut self, device: &str, paths: Vec<String>) {
        let (Some(index), Some(root)) = (self.index_of(device, BUCKET_ID), self.bucket_dir(device))
        else {
            return;
        };
        let rows = crate::device_links::webrtc::transfer_board::synced_rows();
        let live = &mut self.pairs[index];
        let mut removed = false;
        for path in paths.iter().take(MAX_BUCKET_ITEMS) {
            if !bucket_path_ok(path, true) && !bucket_path_ok(path, false) {
                continue;
            }
            let below = format!("{path}/");
            for row in rows
                .iter()
                .filter(|row| row.device_id == device)
                .filter(|row| row.name == *path || row.name.starts_with(&below))
            {
                let _ = crate::device_links::desk_features::cancel_transfer(device, &row.id);
            }
            live.send_queue
                .retain(|queued| queued != path && !queued.starts_with(&below));
            live.flight
                .retain(|sent, _| sent != path && !sent.starts_with(&below));
            live.outstanding
                .retain(|asked| asked != path && !asked.starts_with(&below));
            live.to_request
                .retain(|asked| asked != path && !asked.starts_with(&below));
            // Files of it not fetched yet: gone for the phone too, or the
            // next manifest would bring them back.
            let unfetched: Vec<String> = live
                .receiving
                .keys()
                .filter(|known| *known == path || known.starts_with(&below))
                .cloned()
                .collect();
            for known in unfetched {
                let Some(theirs) = live.receiving.remove(&known) else {
                    continue;
                };
                live.remote.remove(&known);
                live.sending.remove(&known);
                if let Ok(mut wanted) = WANTED.lock() {
                    wanted.remove(&(device.to_string(), live.pair.id.clone(), known.clone()));
                }
                live.book.manifest.insert(
                    known,
                    Entry {
                        size: 0,
                        mtime: now_ms(),
                        sha256: String::new(),
                        deleted: true,
                        dir: false,
                        vv: theirs.vv.bumped(Side::Desk),
                    },
                );
                removed = true;
            }
            if fs::symlink_metadata(root.join(path)).is_ok() {
                removed |= trash(&root.join(path));
            }
        }
        // Everything removed on purpose: the empty folder is meant, so the
        // next scan tells the phone (it is not an unmounted drive).
        if removed && scan(&live.pair).is_ok_and(|(found, ..)| found.is_empty()) {
            live.allow_empty = true;
        }
        live.dirty = Some(Instant::now() - SETTLE);
        self.save_book(index);
        self.publish();
    }
}
