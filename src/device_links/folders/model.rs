//! The sync model and its planner: version vectors, manifest entries, path and
//! name rules, conflict names, [`plan`] and [`apply_scan`]. No I/O and no
//! threads (glib only formats the date in a conflict name), so it is tested
//! directly and by `property_tests`.
//!
//! Sibling edges: `state` (none other). Outside: `file_protocol`
//! (`validate_relpath`, `is_bidi`); the parent's limits.

use super::*;

// ── The model ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Side {
    Desk,
    Phone,
}

/// One counter per side: each bumps its own on a local change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Vv {
    pub(crate) desk: u64,
    pub(crate) phone: u64,
}

impl Vv {
    pub(super) fn dominates(self, other: Vv) -> bool {
        self.desk >= other.desk && self.phone >= other.phone && self != other
    }

    pub(super) fn max(self, other: Vv) -> Vv {
        Vv {
            desk: self.desk.max(other.desk),
            phone: self.phone.max(other.phone),
        }
    }

    pub(super) fn bumped(mut self, side: Side) -> Vv {
        match side {
            Side::Desk => self.desk = self.desk.saturating_add(1),
            Side::Phone => self.phone = self.phone.saturating_add(1),
        }
        self
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

/// A name another system (Windows, SAF) can hold too: bounded, with no
/// `\` or `:`, no control characters, no trailing dot or space; and no bidi
/// override (`invoice<U+202E>fdp.exe` shows as `invoiceexe.pdf`), which
/// Downloads strips too (`sanitize_filename`).
pub(crate) fn name_ok(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_NAME_BYTES
        && !matches!(segment, "." | "..")
        && !segment.ends_with(['.', ' '])
        && !segment.chars().any(|c| {
            c.is_control()
                || crate::device_links::webrtc::file_protocol::is_bidi(c)
                || matches!(c, '/' | '\\' | ':')
        })
}

/// A path the Bucket holds: valid names, at most [`MAX_BUCKET_DEPTH`] folder
/// levels (a folder `A/B`, a file `A/B/c`).
pub(super) fn bucket_path_ok(path: &str, dir: bool) -> bool {
    let segments = path.split('/').count();
    validate_relpath(path).is_ok()
        && segments <= MAX_BUCKET_DEPTH + usize::from(!dir)
        && path.split('/').all(name_ok)
}

/// The folder `path` is in ("" for the top).
pub(crate) fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// Folder levels a path has above it (`a` 0, `A/a` 1).
pub(super) fn levels(path: &str) -> usize {
    path.matches('/').count()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Entry {
    #[serde(default)]
    pub(crate) size: u64,
    #[serde(default)]
    pub(crate) mtime: i64,
    #[serde(default)]
    pub(crate) sha256: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) deleted: bool,
    /// F8c: a folder (`dir: true` on the wire): no size or hash; only that
    /// it exists.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) dir: bool,
    pub(crate) vv: Vv,
}

pub(super) type Manifest = BTreeMap<String, Entry>;

/// A local file's size and mtime (ms), from a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Stat {
    pub(crate) size: u64,
    pub(crate) mtime: i64,
    /// A folder.
    pub(crate) dir: bool,
}

/// One step of a plan, for the side that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Step {
    /// Record the other side's entry (a tombstone, or the same content).
    Take { path: String, entry: Entry },
    /// Remove the local file; record `entry` (a tombstone).
    Delete { path: String, entry: Entry },
    /// Fetch the other side's file at `path`.
    Download { path: String, entry: Entry },
    /// A local file that `Delete` would remove has the content to fetch.
    Move {
        from: String,
        to: String,
        entry: Entry,
        tombstone: Entry,
    },
    /// Another local file has the content to fetch.
    Copy {
        from: String,
        to: String,
        entry: Entry,
    },
    /// Ours wins a concurrent change: our entry gets `vv`, so the other
    /// side takes it.
    Bump { path: String, vv: Vv },
    /// The phone's own version moves aside under the conflict name.
    Rename { from: String, to: String },
    /// A conflict to list (the desk's version kept, the phone's copy named).
    Conflict { path: String, copy: String },
}

/// `<stem> (conflict from <phone> <YYYY-MM-DD>)<.ext>`, beside `path`; the
/// date is `mtime`'s, UTC.
pub(super) fn conflict_name(path: &str, phone: &str, mtime: i64) -> String {
    let (dir, name) = match path.rsplit_once('/') {
        Some((dir, name)) => (format!("{dir}/"), name),
        None => (String::new(), path),
    };
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && extension.len() <= 16 => {
            (stem, format!(".{extension}"))
        }
        _ => (name, String::new()),
    };
    let date = gtk::glib::DateTime::from_unix_utc(mtime.div_euclid(1000))
        .ok()
        .and_then(|date| date.format("%Y-%m-%d").ok())
        .map(|date| date.to_string())
        .unwrap_or_else(|| "1970-01-01".into());
    let phone: String = phone
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | '\0' | ':') && !c.is_control())
        .take(40)
        .collect();
    let suffix = format!(" (conflict from {phone} {date}){extension}");
    // The name stays a valid filename (≤ 255 bytes).
    let mut stem = stem.to_string();
    while stem.len() + suffix.len() > 255 && stem.pop().is_some() {}
    format!("{dir}{stem}{suffix}")
}

/// P3: what `local_side` does with the other side's complete manifest.
pub(super) fn plan(
    local_side: Side,
    local: &Manifest,
    remote: &Manifest,
    phone_name: &str,
) -> Vec<Step> {
    let mut steps = Vec::new();
    for (path, theirs) in remote {
        match local.get(path) {
            None if theirs.deleted => steps.push(Step::Take {
                path: path.clone(),
                entry: theirs.clone(),
            }),
            None => steps.push(Step::Download {
                path: path.clone(),
                entry: theirs.clone(),
            }),
            Some(ours) if ours.vv == theirs.vv => {}
            Some(ours) if theirs.vv.dominates(ours.vv) => {
                let same = ours.sha256 == theirs.sha256 && ours.deleted == theirs.deleted;
                steps.push(if same || (ours.deleted && theirs.deleted) {
                    Step::Take {
                        path: path.clone(),
                        entry: theirs.clone(),
                    }
                } else if theirs.deleted {
                    Step::Delete {
                        path: path.clone(),
                        entry: theirs.clone(),
                    }
                } else {
                    Step::Download {
                        path: path.clone(),
                        entry: theirs.clone(),
                    }
                });
            }
            Some(ours) if ours.vv.dominates(theirs.vv) => {}
            Some(ours) => {
                // Concurrent.
                let merged = ours.vv.max(theirs.vv);
                if (ours.deleted && theirs.deleted)
                    || (!ours.deleted && !theirs.deleted && ours.sha256 == theirs.sha256)
                {
                    steps.push(Step::Take {
                        path: path.clone(),
                        entry: Entry {
                            vv: merged,
                            ..theirs.clone()
                        },
                    });
                } else if ours.deleted {
                    // Their edit beats our deletion.
                    steps.push(Step::Download {
                        path: path.clone(),
                        entry: Entry {
                            vv: merged,
                            ..theirs.clone()
                        },
                    });
                } else if theirs.deleted {
                    steps.push(Step::Bump {
                        path: path.clone(),
                        vv: merged.bumped(local_side),
                    });
                } else {
                    // Both edited: the desk's keeps the name, the phone's
                    // is kept under the conflict name. Only the phone acts
                    // (renames its own, takes the desk's under a version
                    // that dominates both); the desk just lists it — had it
                    // bumped its counter, the phone would see the desk's as
                    // newer and drop its own.
                    let phone_mtime = match local_side {
                        Side::Desk => theirs.mtime,
                        Side::Phone => ours.mtime,
                    };
                    let copy = conflict_name(path, phone_name, phone_mtime);
                    let winner = merged.bumped(Side::Desk);
                    match local_side {
                        Side::Desk => steps.push(Step::Conflict {
                            path: path.clone(),
                            copy,
                        }),
                        Side::Phone => {
                            steps.push(Step::Rename {
                                from: path.clone(),
                                to: copy,
                            });
                            steps.push(Step::Download {
                                path: path.clone(),
                                entry: Entry {
                                    vv: winner,
                                    ..theirs.clone()
                                },
                            });
                        }
                    }
                }
            }
        }
    }
    renames_by_hash(local, steps)
}

/// A download whose content a deleted (or kept) local file already has
/// becomes a move (or a copy): no transfer.
fn renames_by_hash(local: &Manifest, steps: Vec<Step>) -> Vec<Step> {
    let mut deletes: HashMap<String, (String, Entry)> = HashMap::new();
    for step in &steps {
        if let Step::Delete { path, entry } = step {
            if let Some(ours) = local.get(path).filter(|ours| !ours.deleted && !ours.dir) {
                deletes
                    .entry(ours.sha256.clone())
                    .or_insert((path.clone(), entry.clone()));
            }
        }
    }
    let changing: HashSet<&str> = steps
        .iter()
        .filter_map(|step| match step {
            Step::Delete { path, .. }
            | Step::Download { path, .. }
            | Step::Rename { from: path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    let kept: HashMap<&str, &str> = local
        .iter()
        .filter(|(path, entry)| !entry.deleted && !entry.dir && !changing.contains(path.as_str()))
        .map(|(path, entry)| (entry.sha256.as_str(), path.as_str()))
        .collect();
    let mut moved: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for step in steps {
        match step {
            Step::Download { path, entry } if entry.dir => out.push(Step::Download { path, entry }),
            Step::Download { path, entry } => {
                if let Some((from, tombstone)) = deletes.remove(&entry.sha256) {
                    moved.insert(from.clone());
                    out.push(Step::Move {
                        from,
                        to: path,
                        entry,
                        tombstone,
                    });
                } else if let Some(from) = kept.get(entry.sha256.as_str()) {
                    out.push(Step::Copy {
                        from: from.to_string(),
                        to: path,
                        entry,
                    });
                } else {
                    out.push(Step::Download { path, entry });
                }
            }
            other => out.push(other),
        }
    }
    out.retain(|step| !matches!(step, Step::Delete { path, .. } if moved.contains(path)));
    out
}

/// Folds a scan into the manifest for `side`: new or changed files bump
/// its counter (hashing only what changed), vanished ones become
/// tombstones. Whether anything changed.
pub(super) fn apply_scan(
    side: Side,
    manifest: &mut Manifest,
    found: &BTreeMap<String, Stat>,
    hash: &mut dyn FnMut(&str) -> Option<String>,
    now: i64,
) -> bool {
    let mut changed = false;
    for (path, stat) in found {
        if stat.dir {
            let fresh = |vv: Vv| Entry {
                size: 0,
                mtime: 0,
                sha256: String::new(),
                deleted: false,
                dir: true,
                vv,
            };
            match manifest.get_mut(path) {
                Some(entry) if entry.dir && !entry.deleted => {}
                Some(entry) => {
                    *entry = fresh(entry.vv.bumped(side));
                    changed = true;
                }
                None => {
                    if manifest.len() < MAX_FILES {
                        manifest.insert(path.clone(), fresh(Vv::default().bumped(side)));
                        changed = true;
                    }
                }
            }
            continue;
        }
        match manifest.get_mut(path) {
            Some(entry)
                if !entry.deleted
                    && !entry.dir
                    && entry.size == stat.size
                    && entry.mtime == stat.mtime => {}
            existing => {
                let Some(sha256) = hash(path) else {
                    continue;
                };
                match existing {
                    Some(entry) if !entry.deleted && !entry.dir && entry.sha256 == sha256 => {
                        // Touched, not changed.
                        entry.size = stat.size;
                        entry.mtime = stat.mtime;
                    }
                    Some(entry) => {
                        *entry = Entry {
                            size: stat.size,
                            mtime: stat.mtime,
                            sha256,
                            deleted: false,
                            dir: false,
                            vv: entry.vv.bumped(side),
                        };
                    }
                    None => {
                        if manifest.len() >= MAX_FILES {
                            continue;
                        }
                        manifest.insert(
                            path.clone(),
                            Entry {
                                size: stat.size,
                                mtime: stat.mtime,
                                sha256,
                                deleted: false,
                                dir: false,
                                vv: Vv::default().bumped(side),
                            },
                        );
                    }
                }
                changed = true;
            }
        }
    }
    for (path, entry) in manifest.iter_mut() {
        if !entry.deleted && !found.contains_key(path) {
            entry.deleted = true;
            entry.size = 0;
            entry.sha256.clear();
            entry.mtime = now;
            entry.vv = entry.vv.bumped(side);
            changed = true;
        }
    }
    let cutoff = now - TOMBSTONE_DAYS * 24 * 3600 * 1000;
    manifest.retain(|_, entry| !entry.deleted || entry.mtime > cutoff);
    changed
}

/// P5, for one path relative to the root.
pub(super) fn ignored(path: &str, include_hidden: bool, patterns: &[String]) -> bool {
    path.split('/').any(|segment| {
        segment == ".git"
            || segment == "node_modules"
            || segment.starts_with(".desklink")
            || segment.ends_with(".tmp")
            || segment.ends_with(".part")
            || segment.ends_with('~')
            || (!include_hidden && segment.starts_with('.'))
            || patterns
                .iter()
                .any(|pattern| match pattern.strip_prefix('*') {
                    Some(suffix) => !suffix.is_empty() && segment.ends_with(suffix),
                    None => segment == pattern,
                })
    })
}
