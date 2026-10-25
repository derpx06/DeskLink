//! The receiving half: offers, frames, blocks, checkpoints, pauses.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::Ordering;
use std::time::Instant;

use openssl::sha::Sha256;

use super::super::file_protocol::{
    blocks_for, code, hash_of_hashes, hex, id_bytes, next_pass, parse_frame, reason, unhex32,
    FileAction, FileControl, FolderRef, Frame, ACTIVE_PER_DIRECTION, BLOCK_BYTES,
    BUCKET_SLOTS_PER_DIRECTION, FRAME_BYTES_PROVEN, MAX_REWINDS, MAX_TRANSFER_BYTES,
};
use super::store::{is_storage_full, ReceiveCheckpoint, Store};
use super::{
    board, is_clipboard_offer, row_for, words, Completed, Ending, Engine, Faults, Net, Outcome,
    Stall, Tunables,
};

/// Why a receiver holds a transfer still, on its own account.
pub(super) enum Pause {
    /// The disk is full: look again at `retry_at`.
    Storage { retry_at: Instant },
    /// The file thread is behind: resumes when the queue drains.
    Backlog,
}

/// What a receiver needs from the engine to handle one message.
pub(super) struct Ctx<'a> {
    pub net: Option<&'a Net>,
    pub store: &'a Store,
    pub tun: &'a Tunables,
    pub faults: &'a Faults,
    pub now: Instant,
}

/// One incoming file.
pub(super) struct Incoming {
    pub id: String,
    pub token: String,
    device_id: String,
    filename: String,
    destination_name: String,
    total: u64,
    blocks_total: u64,
    mtime: i64,
    folder: Option<FolderRef>,
    content_sha256: Option<String>,
    clipboard: bool,
    capture_id: Option<String>,
    file: File,
    /// The pass frames must carry (see `file_protocol`).
    pass: u8,
    /// After an `accept`: the first frame, whatever its pass, sets it.
    adopt_any: bool,
    /// Where the next frame must start.
    next_offset: u64,
    /// Hashes of the verified blocks.
    hashes: Vec<[u8; 32]>,
    /// The block being received.
    hasher: Option<Sha256>,
    rewinds: u32,
    whole_resends: u32,
    paused: Option<Pause>,
    last_checkpoint: Instant,
    checkpointed_blocks: u64,
    pub stall: Stall,
    last_report: Instant,
    reported: u64,
}

impl Incoming {
    fn verified(&self) -> u64 {
        self.hashes.len() as u64
    }

    /// Verified blocks, for tests.
    #[cfg(test)]
    pub(super) fn verified_blocks(&self) -> u64 {
        self.verified()
    }

    pub fn is_paused(&self) -> bool {
        self.paused.is_some()
    }

    /// Bytes of the file held, verified or not.
    fn progress_bytes(&self) -> u64 {
        self.next_offset.min(self.total)
    }

    fn prefix_hex(&self, blocks: u64) -> String {
        hex(&hash_of_hashes(&self.hashes[..blocks as usize]))
    }

    fn record(&self) -> ReceiveCheckpoint {
        ReceiveCheckpoint {
            transfer_id: self.id.clone(),
            device_id: self.device_id.clone(),
            token: self.token.clone(),
            filename: self.filename.clone(),
            destination_name: self.destination_name.clone(),
            total_size: self.total,
            mtime: self.mtime,
            blocks_done: self.verified(),
            clipboard: self.clipboard,
            capture_id: self.capture_id.clone(),
            folder: self.folder.clone(),
            content_sha256: self.content_sha256.clone(),
        }
    }

    /// Syncs the partial file, then records the verified blocks.
    pub fn checkpoint_now(&mut self, store: &Store) {
        let saved = self
            .file
            .sync_data()
            .map_err(|error| format!("Could not persist DeskLink partial file: {error}"))
            .and_then(|()| store.save_receive(&self.record(), &self.hashes));
        match saved {
            Ok(()) => {
                self.checkpointed_blocks = self.verified();
                self.last_checkpoint = Instant::now();
            }
            Err(error) => crate::device_links::limits::log_limited(
                "file-checkpoint",
                &format!("Could not checkpoint a transfer: {error}"),
            ),
        }
    }

    /// Forgets the transfer: partial file, sidecar, checkpoint.
    pub fn discard(&self, store: &Store) {
        if let Err(error) = store.remove_receive(&self.id) {
            debug_log!("[DeskLink] Could not remove a transfer's files: {error}");
        }
    }

    /// Goes back to the end of block `blocks` (all before it verified) and
    /// expects `pass` from now on: the partial file past it is dropped.
    fn restart_at(&mut self, blocks: u64, pass: u8) -> Result<(), String> {
        let blocks = blocks.min(self.verified());
        let keep = (blocks * BLOCK_BYTES).min(self.total);
        self.file
            .set_len(keep)
            .map_err(|error| format!("Could not repair DeskLink partial file: {error}"))?;
        self.hashes.truncate(blocks as usize);
        self.hasher = None;
        self.next_offset = keep;
        self.pass = pass;
        self.adopt_any = false;
        // The checkpoint never claims more than the file holds.
        self.checkpointed_blocks = self.checkpointed_blocks.min(blocks);
        Ok(())
    }

    /// Tells the sender to begin again at the last verified block with a new
    /// pass (after a pause, a backlog, a dropped frame).
    fn restart_and_resume(&mut self, ctx: &Ctx<'_>) -> Result<(), Ending> {
        let blocks = self.verified();
        self.restart_at(blocks, next_pass(self.pass))
            .map_err(Ending::internal)?;
        self.paused = None;
        self.stall.moved(ctx.now);
        board::set_note(&self.id, None);
        if let Some(net) = ctx.net {
            let mut message = net.msg(FileAction::Resume, &self.id, &self.token);
            message.resume_block = Some(blocks);
            message.prefix_sha256 = Some(self.prefix_hex(blocks));
            message.pass = Some(self.pass);
            net.send(message);
        }
        Ok(())
    }

    /// The sender's reply to our `accept` / a re-offer: where we are.
    fn accept_message(&mut self, net: &Net) -> FileControl {
        let blocks = self.verified();
        let mut accept = net.msg(FileAction::Accept, &self.id, &self.token);
        accept.resume_block = Some(blocks);
        accept.prefix_sha256 = Some(self.prefix_hex(blocks));
        accept.max_frame_bytes = Some(FRAME_BYTES_PROVEN);
        accept
    }

    pub fn report(&mut self, now: Instant, every: std::time::Duration) {
        let done = self.progress_bytes();
        if done != self.reported && now.saturating_duration_since(self.last_report) >= every {
            board::progress(&self.id, done);
            self.reported = done;
            self.last_report = now;
        }
    }

    /// Whether a frame of another pass starts a new one: from a block
    /// boundary at or before the verified end, either right after an `accept`
    /// (any pass) or with the pass after ours (a sender's own rewind).
    fn adopts(&self, frame: &Frame<'_>) -> Option<u64> {
        let header = &frame.header;
        let aligned = header.offset % BLOCK_BYTES == 0;
        let block = header.offset / BLOCK_BYTES;
        if !aligned || block > self.verified() {
            return None;
        }
        (self.adopt_any || header.pass == next_pass(self.pass)).then_some(block)
    }

    /// One frame of this transfer.
    pub fn on_frame(&mut self, ctx: &Ctx<'_>, frame: &Frame<'_>) -> Result<(), Ending> {
        let header = &frame.header;
        // Past the end of the file, or not a frame of this one: dropped, and
        // nothing changes.
        if header.check(self.total).is_err() {
            super::note_dropped("out of range");
            return Ok(());
        }
        if matches!(self.paused, Some(Pause::Storage { .. })) {
            return Ok(());
        }
        if self.adopt_any {
            // The first frame after an `accept`, whatever its pass: from a
            // block boundary at or before the verified end it is the stream
            // beginning, and the sender's word on where it begins. A phone
            // that cannot reproduce the prefix restarts at block 0 under the
            // SAME pass (older builds), so an earlier offset of this pass is
            // not a duplicate here.
            match self.adopts(frame) {
                Some(block) => self
                    .restart_at(block, header.pass)
                    .map_err(Ending::internal)?,
                None if header.pass != self.pass => {
                    super::note_dropped("another pass");
                    return Ok(());
                }
                None => self.adopt_any = false,
            }
        } else if header.pass != self.pass {
            let Some(block) = self.adopts(frame) else {
                super::note_dropped("another pass");
                return Ok(());
            };
            self.restart_at(block, header.pass)
                .map_err(Ending::internal)?;
        }
        if header.offset < self.next_offset {
            super::note_dropped("duplicate");
            return Ok(());
        }
        if header.offset > self.next_offset {
            return Err(Ending {
                code: code::BAD_FRAME,
                words: "The transfer went out of order and was stopped.".to_string(),
                tell_peer: true,
            });
        }
        let written = if ctx.faults.disk_full {
            Err(std::io::Error::from(std::io::ErrorKind::StorageFull))
        } else {
            self.file.write_all_at(frame.payload, header.offset)
        };
        if let Err(error) = written {
            if is_storage_full(&error) {
                self.pause_for_storage(ctx);
                return Ok(());
            }
            return Err(Ending::internal(format!(
                "Could not save the file: {error}"
            )));
        }
        if self.hasher.is_none() {
            if header.offset % BLOCK_BYTES != 0 {
                return Err(Ending::internal("A file block began in the middle"));
            }
            self.hasher = Some(Sha256::new());
        }
        if let Some(hasher) = self.hasher.as_mut() {
            hasher.update(frame.payload);
        }
        self.next_offset += u64::from(header.length);
        self.stall.moved(ctx.now);
        if header.last_of_block {
            self.finish_block(ctx, frame)?;
        }
        Ok(())
    }

    /// The last frame of a block landed: compare, acknowledge or refuse.
    fn finish_block(&mut self, ctx: &Ctx<'_>, frame: &Frame<'_>) -> Result<(), Ending> {
        let index = u64::from(frame.header.block_index);
        let computed = self
            .hasher
            .take()
            .map(Sha256::finish)
            .ok_or_else(|| Ending::internal("A file block had no hash"))?;
        if frame.trailer == Some(computed) {
            self.hashes.push(computed);
            if let Some(net) = ctx.net {
                let mut ack = net.msg(FileAction::BlockAck, &self.id, &self.token);
                ack.block_index = Some(index);
                ack.sha256 = Some(hex(&computed));
                net.send(ack);
            }
            return Ok(());
        }
        // The block arrived damaged: dropped and asked for again, under a new
        // pass so the frames still in flight are ignored.
        self.rewinds += 1;
        if self.rewinds > MAX_REWINDS {
            return Err(Ending {
                code: code::HASH_MISMATCH,
                words: words::damaged(),
                tell_peer: true,
            });
        }
        let blocks = self.verified();
        let pass = next_pass(self.pass);
        self.restart_at(blocks, pass).map_err(Ending::internal)?;
        if let Some(net) = ctx.net {
            let mut nack = net.msg(FileAction::BlockNack, &self.id, &self.token);
            nack.block_index = Some(index);
            nack.pass = Some(pass);
            nack.reason = Some(code::HASH_MISMATCH.to_string());
            net.send(nack);
        }
        Ok(())
    }

    /// The disk is full: what is verified stays, the block in progress goes,
    /// and the sender is asked to wait. Looked at again every few seconds.
    fn pause_for_storage(&mut self, ctx: &Ctx<'_>) {
        let blocks = self.verified();
        let keep = (blocks * BLOCK_BYTES).min(self.total);
        let _ = self.file.set_len(keep);
        self.hasher = None;
        self.next_offset = keep;
        self.paused = Some(Pause::Storage {
            retry_at: ctx.now + ctx.tun.storage_retry,
        });
        self.checkpoint_now(ctx.store);
        board::set_note(&self.id, Some(&words::storage_full()));
        if let Some(net) = ctx.net {
            let mut pause = net.msg(FileAction::Pause, &self.id, &self.token);
            pause.reason = Some(reason::STORAGE_FULL.to_string());
            net.send(pause);
        }
    }
}

impl Engine {
    /// An offer: a new file to receive, a transfer we hold again, or a no.
    pub(super) fn on_offer(&mut self, net: &Net, message: FileControl) {
        let id = message.transfer_id.clone();
        let token = message.transfer_token.clone();
        let refuse = |code: &'static str, text: Option<&str>| {
            let mut error = net.msg(FileAction::Error, &id, &token);
            error.code = Some(code.to_string());
            error.message = text.map(str::to_string);
            net.send(error);
        };
        let (Some(filename), Some(total)) = (message.filename.clone(), message.total_size) else {
            self.refused(
                &message,
                "the offer is incomplete",
                Some("The phone's offer was incomplete."),
            );
            return refuse(code::INTERNAL, Some("The offer is incomplete"));
        };
        if total > MAX_TRANSFER_BYTES {
            self.refused(
                &message,
                "over 4 GiB",
                Some("That file is over 4 GiB, which DeskLink does not receive."),
            );
            return refuse(code::TOO_LARGE, None);
        }
        let Some(key) = id_bytes(&id) else {
            return;
        };
        let folder = message.folder_ref();
        // F8: only a file folder sync asked for, for this phone's pair.
        if let Some(reference) = &folder {
            let content = message.content_sha256.clone().unwrap_or_default();
            if !crate::device_links::folders::accepts(
                &net.wire.peer_device_id,
                reference,
                &content,
                total,
            ) {
                // The Bucket asks again by itself: logged, not listed.
                self.refused(&message, "the Bucket did not ask for it", None);
                return refuse(code::REFUSED, Some("folder-refused"));
            }
        }
        // The same transfer offered again: answered from where we are.
        if let Some(existing) = self.incoming.get_mut(&key) {
            if existing.token != token || existing.total != total || existing.filename != filename {
                self.refused(
                    &message,
                    "it does not match the transfer already open under that id",
                    Some("That file does not match a transfer already open."),
                );
                return refuse(code::REFUSED, Some("That transfer does not match"));
            }
            let blocks = existing.verified();
            if let Err(error) = existing.restart_at(blocks, existing.pass) {
                debug_log!("[DeskLink] {error}");
            }
            existing.adopt_any = true;
            existing.paused = None;
            net.send(existing.accept_message(net));
            return;
        }
        // A Bucket file asked for again under a new id while the old one is
        // still open here (the cut was not noticed, or the link is down):
        // checkpoint the old one and let the new offer adopt what it holds.
        if let (Some(reference), Some(sha)) = (&folder, message.content_sha256.as_deref()) {
            let same_file = |incoming: &Incoming| {
                incoming.device_id == net.wire.peer_device_id
                    && incoming.folder.as_ref().is_some_and(|known| {
                        known.folder == reference.folder && known.relpath == reference.relpath
                    })
                    && incoming
                        .content_sha256
                        .as_deref()
                        .is_some_and(|known| known.eq_ignore_ascii_case(sha))
                    && incoming.total == total
            };
            let stale: Vec<[u8; 16]> = self
                .incoming
                .iter()
                .filter(|(old, incoming)| **old != key && same_file(incoming))
                .map(|(old, _)| *old)
                .collect();
            for old in stale {
                if let Some(mut incoming) = self.incoming.remove(&old) {
                    incoming.checkpoint_now(&self.store);
                    board::discard(&incoming.id);
                    self.note_closed(old, "replaced by a new request for the same file");
                }
            }
        }
        // Two at once; the sender waits and offers again.
        let bucket_active = self
            .incoming
            .values()
            .filter(|incoming| {
                // Another file of the Bucket; an older copy of this very path
                // is being replaced by this offer, not competing with it.
                incoming.folder.as_ref().is_some_and(|known| {
                    folder
                        .as_ref()
                        .is_none_or(|reference| known.relpath != reference.relpath)
                })
            })
            .count();
        // A board with no room refuses (the sender waits and offers again)
        // rather than run a transfer nobody can see or cancel.
        let unlisted = folder.is_none() && !is_clipboard_offer(&message) && !board::has_room();
        if self.incoming.len() >= ACTIVE_PER_DIRECTION
            || (folder.is_some() && bucket_active >= BUCKET_SLOTS_PER_DIRECTION)
            || unlisted
        {
            // Not a failure: the sender waits and offers again.
            self.refused(&message, "two files are already arriving (busy)", None);
            return refuse(code::BUSY, None);
        }
        let now = Instant::now();
        let incoming = match self.begin_incoming(net, &message, filename, total, folder, now) {
            Ok(incoming) => incoming,
            Err(error) => {
                debug_log!("[DeskLink] Refusing offer {id}: {error}");
                board::record(board::Finished {
                    id: id.clone(),
                    device_id: net.wire.peer_device_id.clone(),
                    name: message.filename.clone().unwrap_or_default(),
                    incoming: true,
                    outcome: Outcome::Failed(error.chars().take(256).collect()),
                    retry: None,
                });
                self.note_closed(key, "refused");
                return refuse(code::INTERNAL, Some(&error));
            }
        };
        self.incoming.insert(key, incoming);
        if let Some(incoming) = self.incoming.get_mut(&key) {
            net.send(incoming.accept_message(net));
        }
    }

    /// Opens the files of a new incoming transfer, resuming a checkpoint of
    /// the same transfer if there is one.
    fn begin_incoming(
        &mut self,
        net: &Net,
        message: &FileControl,
        filename: String,
        total: u64,
        folder: Option<FolderRef>,
        now: Instant,
    ) -> Result<Incoming, String> {
        let id = &message.transfer_id;
        let mtime = message.mtime.unwrap_or_default();
        let mut held = self.store.load_receive(id)?;
        // A Bucket file asked for again under a new id carries on from what
        // is held for the same content (see "Bucket resume" in WEBRTC.md).
        if let (None, Some(folder_ref), Some(sha)) =
            (&held, &folder, message.content_sha256.as_deref())
        {
            if self.store.adopt_bucket_partial(
                id,
                &message.transfer_token,
                &net.wire.peer_device_id,
                folder_ref,
                sha,
                total,
                mtime,
            )? {
                held = self.store.load_receive(id)?;
            }
        }
        let resumed = held
            .filter(|(checkpoint, _)| {
                let same_file = checkpoint.device_id == net.wire.peer_device_id
                    && checkpoint.filename == filename
                    && checkpoint.total_size == total;
                match (&folder, &checkpoint.folder) {
                    // A Bucket file is its content at its path: not the token or
                    // the time of the request ("Bucket resume", WEBRTC.md).
                    (Some(new), Some(old)) => {
                        same_file
                            && new.folder == old.folder
                            && new.relpath == old.relpath
                            && checkpoint.content_sha256.as_deref().is_some_and(|known| {
                                message
                                    .content_sha256
                                    .as_deref()
                                    .is_some_and(|asked| known.eq_ignore_ascii_case(asked))
                            })
                    }
                    (None, None) => {
                        same_file
                            && checkpoint.token == message.transfer_token
                            && checkpoint.mtime == mtime
                    }
                    _ => false,
                }
            })
            .map(|(mut checkpoint, hashes)| {
                if folder.is_some() {
                    // Carried on under this request.
                    checkpoint.token = message.transfer_token.clone();
                    checkpoint.mtime = mtime;
                    checkpoint.folder = folder.clone();
                }
                (checkpoint, hashes)
            });
        let (checkpoint, hashes, file) = match resumed {
            Some((checkpoint, hashes)) => {
                let file = self.store.open_part(id)?;
                // What the checkpoint proves, no more.
                file.set_len((checkpoint.blocks_done * BLOCK_BYTES).min(total))
                    .and_then(|()| file.sync_all())
                    .map_err(|error| format!("Could not repair DeskLink partial file: {error}"))?;
                (checkpoint, hashes, file)
            }
            None => {
                // Whatever else is under this id is nobody's now.
                self.store.remove_receive(id)?;
                let file = self.store.create_part(id)?;
                let checkpoint = ReceiveCheckpoint {
                    transfer_id: id.clone(),
                    device_id: net.wire.peer_device_id.clone(),
                    token: message.transfer_token.clone(),
                    destination_name: self.store.available_destination_name(&filename)?,
                    filename,
                    total_size: total,
                    mtime,
                    blocks_done: 0,
                    clipboard: is_clipboard_offer(message),
                    capture_id: message
                        .capture_id
                        .clone()
                        .filter(|id| uuid::Uuid::parse_str(id).is_ok()),
                    folder: folder.clone(),
                    content_sha256: message.content_sha256.clone(),
                };
                (checkpoint, Vec::new(), file)
            }
        };
        let blocks = checkpoint.blocks_done;
        let mut incoming = Incoming {
            id: id.clone(),
            token: checkpoint.token.clone(),
            device_id: checkpoint.device_id.clone(),
            filename: checkpoint.filename.clone(),
            destination_name: checkpoint.destination_name.clone(),
            total,
            blocks_total: blocks_for(total),
            mtime,
            folder: checkpoint.folder.clone(),
            content_sha256: checkpoint.content_sha256.clone(),
            clipboard: checkpoint.clipboard,
            capture_id: checkpoint.capture_id.clone(),
            file,
            pass: 0,
            adopt_any: true,
            next_offset: (blocks * BLOCK_BYTES).min(total),
            hashes,
            hasher: None,
            rewinds: 0,
            whole_resends: 0,
            paused: None,
            last_checkpoint: now,
            checkpointed_blocks: blocks,
            stall: Stall::new(now),
            last_report: now,
            reported: 0,
        };
        incoming.checkpoint_now(&self.store);
        // A clipboard image or a synced file is not a download: no row on the
        // Transfers page (a synced file has the Bucket's own).
        let row = row_for(
            &net.wire,
            id,
            &incoming.filename,
            true,
            total,
            incoming.progress_bytes(),
        );
        match &incoming.folder {
            Some(folder) => board::begin_synced(super::TransferRow {
                name: folder.relpath.clone(),
                ..row
            }),
            None if !incoming.clipboard => {
                board::begin(row);
            }
            None => {}
        }
        Ok(incoming)
    }

    /// One data-channel message.
    pub(super) fn on_frame(&mut self, bytes: &[u8]) {
        let Ok(frame) = parse_frame(bytes) else {
            super::note_dropped("malformed");
            return;
        };
        if !super::super::file_protocol::message_fits(bytes.len()) {
            super::note_dropped("over the frame size");
            return;
        }
        let key = frame.header.transfer;
        let ctx = Ctx {
            net: self.net.as_deref(),
            store: &self.store,
            tun: &self.tun,
            faults: &self.faults,
            now: Instant::now(),
        };
        if !self.incoming.contains_key(&key) {
            // Unknown, finished or cancelled: nothing changes, but the log
            // says which transfer and why it is not known.
            let why = self.why_unknown(&key);
            super::note_dropped(&format!(
                "unknown transfer {}: {why}",
                super::super::file_protocol::id_string(&key)
            ));
            return;
        }
        let Some(incoming) = self.incoming.get_mut(&key) else {
            return;
        };
        if let Err(ending) = incoming.on_frame(&ctx, &frame) {
            self.end_incoming(key, ending);
        }
    }

    /// The sender says every block was acknowledged: verify the whole file
    /// from the block hashes (never from the file) and publish it.
    pub(super) fn on_done(&mut self, net: &Net, message: &FileControl) {
        let Some(key) = id_bytes(&message.transfer_id) else {
            return;
        };
        let Some(incoming) = self.incoming.get_mut(&key) else {
            return;
        };
        if incoming.token != message.transfer_token {
            return;
        }
        let expected = hex(&hash_of_hashes(&incoming.hashes));
        let consistent = message.blocks == Some(incoming.blocks_total)
            && message.total_size == Some(incoming.total)
            && incoming.verified() == incoming.blocks_total;
        if !consistent {
            return self.end_incoming(
                key,
                Ending {
                    code: code::BAD_FRAME,
                    words: "The transfer ended before all of the file arrived.".to_string(),
                    tell_peer: true,
                },
            );
        }
        let matches = message
            .file_hash
            .as_deref()
            .and_then(unhex32)
            .zip(unhex32(&expected))
            .is_some_and(|(theirs, ours)| theirs == ours);
        if matches {
            return self.finish_incoming(net, key);
        }
        // The sender's list and ours disagree although every block matched:
        // the whole file is sent once more, then it is a clear failure.
        if incoming.whole_resends >= 1 {
            return self.end_incoming(
                key,
                Ending {
                    code: code::HASH_MISMATCH,
                    words: words::damaged(),
                    tell_peer: true,
                },
            );
        }
        incoming.whole_resends += 1;
        let restarted = incoming
            .restart_at(0, next_pass(incoming.pass))
            .map_err(Ending::internal)
            .map(|()| {
                let mut resume = net.msg(FileAction::Resume, &incoming.id, &incoming.token);
                resume.resume_block = Some(0);
                resume.prefix_sha256 = Some(hex(&hash_of_hashes(&[])));
                resume.pass = Some(incoming.pass);
                net.send(resume);
            });
        if let Err(ending) = restarted {
            self.end_incoming(key, ending);
        }
    }

    /// Places a fully verified file where it belongs and tells the sender.
    fn finish_incoming(&mut self, net: &Net, key: [u8; 16]) {
        let Some(incoming) = self.incoming.remove(&key) else {
            return;
        };
        self.note_closed(key, "already finished");
        if let Err(error) = incoming.file.sync_all() {
            return self.fail_incoming_keeping_part(
                incoming,
                Ending::internal(format!("Could not persist DeskLink download: {error}")),
            );
        }
        let id = incoming.id.clone();
        let placed = self.place(&incoming);
        let path = match placed {
            Ok(path) => path,
            // The file is whole and verified: the sender is told it was not
            // placed (never "complete"), and the part and its checkpoint stay,
            // so the same transfer offered again only has to be placed.
            Err(error) => {
                return self.fail_incoming_keeping_part(incoming, Ending::internal(error));
            }
        };
        let mut complete = net.msg(FileAction::Complete, &id, &incoming.token);
        complete.file_hash = Some(hex(&hash_of_hashes(&incoming.hashes)));
        net.send(complete);
        if let Err(error) = self.store.remove_receive(&id) {
            debug_log!("[DeskLink] Could not remove a finished transfer's files: {error}");
        }
        board::finish(&id, Outcome::Received(path.clone()));
        self.announce(Completed {
            transfer_id: id,
            path,
            capture_id: incoming.capture_id.clone(),
            incoming: true,
        });
    }

    /// Hands the finished partial file to whoever wanted it: folder sync, the
    /// clipboard, or the downloads directory.
    fn place(&self, incoming: &Incoming) -> Result<std::path::PathBuf, String> {
        let id = &incoming.id;
        if let Some(folder) = &incoming.folder {
            // F8: stashed beside the state (one rename), then folder sync
            // puts it in place on its own thread.
            let stash = self.store.stash_path(id)?;
            std::fs::rename(self.store.part_path(id)?, &stash)
                .map_err(|error| format!("Could not stash a synced file: {error}"))?;
            crate::device_links::folders::arrived(
                &incoming.device_id,
                folder.clone(),
                stash.clone(),
                incoming.content_sha256.clone().unwrap_or_default(),
                incoming.total,
            );
            return Ok(stash);
        }
        if incoming.clipboard {
            // Placed on the clipboard from memory; the file goes at once.
            let bytes = std::fs::read(self.store.part_path(id)?)
                .map_err(|error| format!("Could not read DeskLink clipboard image: {error}"))?;
            crate::device_links::daemon::clipboard::image_from_phone(
                &bytes,
                incoming.capture_id.is_some(),
            );
            return Ok(std::path::PathBuf::from(&incoming.filename));
        }
        let placed = self
            .store
            .publish(id, &incoming.destination_name, &incoming.filename)?;
        // The file keeps its own time: a photo taken last year is not "new".
        apply_sender_mtime(&placed, incoming.mtime);
        Ok(placed)
    }

    /// An incoming transfer ends badly: the sender is told, the files go and
    /// the person is shown why.
    pub(super) fn end_incoming(&mut self, key: [u8; 16], ending: Ending) {
        if let Some(incoming) = self.incoming.remove(&key) {
            self.note_closed(key, "ended with an error");
            self.fail_incoming(incoming, ending);
        }
    }

    /// Like [`fail_incoming`](Self::fail_incoming) for a file that arrived
    /// whole but could not be placed: the sender is told, the person is shown
    /// why, and the verified partial file and its checkpoint are kept.
    fn fail_incoming_keeping_part(&mut self, mut incoming: Incoming, ending: Ending) {
        debug_log!(
            "[DeskLink] Could not place incoming transfer {}: {}",
            incoming.id,
            ending.words
        );
        if let Some(net) = &self.net {
            let mut error = net.msg(FileAction::Error, &incoming.id, &incoming.token);
            error.code = Some(ending.code.to_string());
            error.message = Some(ending.words.chars().take(256).collect());
            net.send(error);
        }
        incoming.checkpoint_now(&self.store);
        board::finish(&incoming.id, Outcome::Failed(ending.words));
    }

    fn fail_incoming(&mut self, incoming: Incoming, ending: Ending) {
        debug_log!(
            "[DeskLink] Ending incoming transfer {}: {}",
            incoming.id,
            ending.words
        );
        if ending.tell_peer {
            if let Some(net) = &self.net {
                let mut error = net.msg(FileAction::Error, &incoming.id, &incoming.token);
                error.code = Some(ending.code.to_string());
                error.message = Some(ending.words.chars().take(256).collect());
                net.send(error);
            }
        }
        let id = incoming.id.clone();
        incoming.discard(&self.store);
        board::finish(&id, Outcome::Failed(ending.words));
    }

    /// Frames were dropped because the queue was full: every receiver goes
    /// back to its last verified block, under a new pass.
    pub(super) fn restart_after_overflow(&mut self) {
        if !self.shared.overflow.swap(false, Ordering::AcqRel) {
            return;
        }
        let now = Instant::now();
        let ctx = Ctx {
            net: self.net.as_deref(),
            store: &self.store,
            tun: &self.tun,
            faults: &self.faults,
            now,
        };
        let mut failed = Vec::new();
        for (key, incoming) in &mut self.incoming {
            if matches!(incoming.paused, Some(Pause::Storage { .. })) {
                continue;
            }
            if let Err(ending) = incoming.restart_and_resume(&ctx) {
                failed.push((*key, ending));
            }
        }
        for (key, ending) in failed {
            self.end_incoming(key, ending);
        }
    }

    /// The queue of unhandled frames is long: ask the senders to wait; when it
    /// has drained, restart them at the last verified block.
    pub(super) fn apply_backlog(&mut self) {
        let queued = self.shared.queued.load(Ordering::Acquire);
        let now = Instant::now();
        let ctx = Ctx {
            net: self.net.as_deref(),
            store: &self.store,
            tun: &self.tun,
            faults: &self.faults,
            now,
        };
        let mut failed = Vec::new();
        for (key, incoming) in &mut self.incoming {
            match incoming.paused {
                None if queued >= ctx.tun.queue_pause => {
                    incoming.paused = Some(Pause::Backlog);
                    if let Some(net) = ctx.net {
                        let mut pause = net.msg(FileAction::Pause, &incoming.id, &incoming.token);
                        pause.reason = Some(reason::BACKLOG.to_string());
                        net.send(pause);
                    }
                }
                Some(Pause::Backlog) if queued <= ctx.tun.queue_resume => {
                    if let Err(ending) = incoming.restart_and_resume(&ctx) {
                        failed.push((*key, ending));
                    }
                }
                _ => {}
            }
        }
        for (key, ending) in failed {
            self.end_incoming(key, ending);
        }
    }

    /// A transfer paused for lack of space looks at the disk again.
    pub(super) fn retry_after_storage_full(&mut self, now: Instant) {
        // `statvfs` only when a paused transfer is due to look (it used to run
        // on every pass of the loop, whatever was going on).
        let due = self.incoming.values().any(|incoming| {
            matches!(incoming.paused, Some(Pause::Storage { retry_at }) if now >= retry_at)
        });
        if !due {
            return;
        }
        let free = self.faults.free_bytes.or_else(|| self.store.free_bytes());
        let wanted = BLOCK_BYTES + self.tun.storage_slack;
        let ctx = Ctx {
            net: self.net.as_deref(),
            store: &self.store,
            tun: &self.tun,
            faults: &self.faults,
            now,
        };
        let mut failed = Vec::new();
        for (key, incoming) in &mut self.incoming {
            let Some(Pause::Storage { retry_at }) = incoming.paused else {
                continue;
            };
            if now < retry_at {
                continue;
            }
            if free.is_some_and(|free| free >= wanted) && !ctx.faults.disk_full {
                if let Err(ending) = incoming.restart_and_resume(&ctx) {
                    failed.push((*key, ending));
                }
            } else {
                incoming.paused = Some(Pause::Storage {
                    retry_at: now + ctx.tun.storage_retry,
                });
            }
        }
        for (key, ending) in failed {
            self.end_incoming(key, ending);
        }
    }

    /// Checkpoints whoever has verified enough blocks or waited long enough.
    pub(super) fn checkpoint_due(&mut self, now: Instant) {
        let (blocks, after) = (self.tun.checkpoint_blocks, self.tun.checkpoint_after);
        for incoming in self.incoming.values_mut() {
            let new = incoming
                .verified()
                .saturating_sub(incoming.checkpointed_blocks);
            if new > 0
                && (new >= blocks
                    || now.saturating_duration_since(incoming.last_checkpoint) >= after)
            {
                incoming.checkpoint_now(&self.store);
            }
        }
    }

    /// Transfers being received, for tests and diagnostics.
    #[cfg(test)]
    pub(super) fn incoming_ids(&self) -> Vec<String> {
        self.incoming
            .keys()
            .map(super::super::file_protocol::id_string)
            .collect()
    }
}

/// Sets `path`'s modification time to the sender's (`ms` since the epoch),
/// when it is a plausible one: not zero, not more than a day ahead. Best
/// effort; the download is already in place.
fn apply_sender_mtime(path: &std::path::Path, ms: i64) {
    let Ok(ms) = u64::try_from(ms) else {
        return;
    };
    if ms == 0 {
        return;
    }
    let time = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms);
    if time > std::time::SystemTime::now() + std::time::Duration::from_secs(24 * 3600) {
        return;
    }
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = file.set_modified(time);
    }
}
