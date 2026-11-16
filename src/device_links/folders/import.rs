//! Adding things to a Bucket: collecting dropped files and folders within the
//! limits, copying them in, pasted bytes, text and links, and safe names.
//!
//! Sibling edges: `model`, `disk`, `report`. Outside: `limits`, `file_protocol`.

use super::*;

/// One thing to copy into the Bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Add {
    src: PathBuf,
    pub(super) rel: String,
    dir: bool,
    size: u64,
}

/// Entries under `dir`, counted (links not followed), up to `cap`.
pub(super) fn count_below(dir: &Path, cap: usize) -> usize {
    let mut count = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            count += 1;
            if count >= cap {
                return cap;
            }
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                stack.push(entry.path());
            }
        }
    }
    count
}

struct Collector<'a> {
    adds: Vec<Add>,
    report: &'a mut Report,
    used_bytes: u64,
    used_items: usize,
    bytes: u64,
}

impl Collector<'_> {
    /// Whether one more item of `size` fits; says why when it does not.
    fn room(&mut self, size: u64) -> bool {
        if self.adds.len() >= MAX_ADD_ITEMS || self.used_items + self.adds.len() >= MAX_BUCKET_ITEMS
        {
            self.report.over_items += 1;
        } else if self.bytes + size > MAX_ADD_BYTES {
            self.report.over_bytes += 1;
        } else if self.used_bytes + self.bytes + size > BUCKET_FULL_BYTES {
            self.report.full = true;
        } else {
            return true;
        }
        false
    }

    fn walk(&mut self, src: &Path, rel: &str) {
        let Ok(metadata) = fs::symlink_metadata(src) else {
            return;
        };
        // Links and special files are always skipped.
        let name = rel.rsplit('/').next().unwrap_or(rel);
        if metadata.file_type().is_symlink() || !(metadata.is_dir() || metadata.is_file()) {
            return;
        }
        if !name_ok(name) {
            self.report.bad_names += 1;
            return;
        }
        if ignored(rel, false, &[]) {
            self.report.hidden += 1;
            return;
        }
        if metadata.is_dir() {
            if levels(rel) + 1 > MAX_BUCKET_DEPTH {
                self.report.too_deep += 1 + count_below(src, MAX_BUCKET_ITEMS);
                return;
            }
            if !self.room(0) {
                return;
            }
            self.adds.push(Add {
                src: src.to_path_buf(),
                rel: rel.to_string(),
                dir: true,
                size: 0,
            });
            let Ok(children) = fs::read_dir(src) else {
                return;
            };
            let mut names: Vec<String> = children
                .flatten()
                .filter_map(|child| child.file_name().to_str().map(str::to_string))
                .collect();
            names.sort();
            for child in names {
                self.walk(&src.join(&child), &format!("{rel}/{child}"));
            }
        } else if metadata.len() > MAX_FILE_BYTES {
            self.report.too_big += 1;
        } else if self.room(metadata.len()) {
            self.bytes += metadata.len();
            self.adds.push(Add {
                src: src.to_path_buf(),
                rel: rel.to_string(),
                dir: false,
                size: metadata.len(),
            });
        }
    }
}

/// What to copy from `sources` into folder `into` of the Bucket at `dest_root`
/// (a folder is copied with what is in it, within the depth limit; the
/// limits of one add and of the Bucket are applied; every left-out item is
/// counted in `report`).
pub(super) fn collect(
    sources: &[PathBuf],
    dest_root: &Path,
    into: &str,
    used: (u64, usize),
    report: &mut Report,
) -> Vec<Add> {
    let mut collector = Collector {
        adds: Vec::new(),
        report,
        used_bytes: used.0,
        used_items: used.1,
        bytes: 0,
    };
    let dest = if into.is_empty() {
        dest_root.to_path_buf()
    } else {
        dest_root.join(into)
    };
    let mut taken = HashSet::new();
    for source in sources.iter().take(MAX_BUCKET_ITEMS) {
        // Never the Bucket into itself.
        if source.starts_with(dest_root) || dest_root.starts_with(source) {
            continue;
        }
        let Some(name) = source.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let target = unique_among(&dest, name, &taken);
        taken.insert(target.clone());
        let Some(unique) = target.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let rel = if into.is_empty() {
            unique.to_string()
        } else {
            format!("{into}/{unique}")
        };
        collector.walk(source, &rel);
    }
    collector.adds
}

/// Writes `adds` under `dest_root`; how many were written and how many
/// failed.
pub(super) fn copy_adds(adds: &[Add], dest_root: &Path) -> (usize, usize) {
    let mut written = 0;
    let mut failed = 0;
    for add in adds {
        let target = dest_root.join(&add.rel);
        let done = if add.dir {
            fs::create_dir(&target).is_ok() || target.is_dir()
        } else {
            let staged =
                target.with_file_name(format!(".desklink-{}.tmp", uuid::Uuid::new_v4().simple()));
            let mtime = fs::metadata(&add.src)
                .map(|meta| mtime_ms(&meta))
                .unwrap_or(0);
            fs::copy(&add.src, &staged).is_ok() && put_in_place(&staged, &target, mtime).is_ok()
        };
        if done {
            written += 1;
        } else {
            failed += 1;
            log_limited("import", "Could not copy an item into the Bucket");
        }
    }
    (written, failed)
}

/// F8b: saves pasted `bytes` as `name` in `device_id`'s Bucket. Text and
/// links over [`MAX_TEXT_ITEM_BYTES`] are refused here.
pub(crate) fn import_bytes(
    device_id: &str,
    name: &str,
    bytes: Vec<u8>,
    into: &str,
) -> Result<(), String> {
    let text = name.ends_with(".txt") || name.ends_with(".url");
    if text && bytes.len() > MAX_TEXT_ITEM_BYTES {
        return Err("That text is too long for the Bucket (1 MiB at most)".into());
    }
    if bytes.is_empty() {
        return Err("Nothing to add".into());
    }
    // Said now, so the toast is truthful: the worker checks again.
    if !name_ok(name) || !(into.is_empty() || bucket_path_ok(into, true)) {
        return Err("That name cannot be used".into());
    }
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("Files over 1 GiB can't go in the Bucket".into());
    }
    send_message(Message::ImportBytes {
        device: device_id.to_string(),
        name: name.to_string(),
        bytes,
        into: into.to_string(),
    });
    Ok(())
}

/// A name from `raw` that [`name_ok`] accepts: what it rejects becomes `-`,
/// nothing trails with a dot or space, never empty.
pub(super) fn clean_name(raw: &str, fallback: &str) -> String {
    let mut name: String = raw
        .chars()
        .map(|c| {
            if c.is_control()
                || crate::device_links::webrtc::file_protocol::is_bidi(c)
                || matches!(c, '/' | '\\' | ':')
            {
                '-'
            } else {
                c
            }
        })
        .collect();
    while name.len() > MAX_NAME_BYTES {
        name.pop();
    }
    let name = name.trim_end_matches(['.', ' ']).trim_start();
    if name.is_empty() || matches!(name, "." | "..") {
        fallback.to_string()
    } else {
        name.to_string()
    }
}

/// The host of a pasted link as a file name: no `user@`, no `:port`, an IPv6
/// address without its brackets (`[::1]` → `1`).
fn link_host(link: &str) -> String {
    let authority = link
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = match host.strip_prefix('[') {
        Some(inside) => inside.split(']').next().unwrap_or(inside),
        None => host.rsplit_once(':').map_or(host, |(name, _port)| name),
    };
    let host: String = host.chars().take(80).collect();
    let name = clean_name(&host, "Link");
    match name.trim_matches('-') {
        "" => "Link".to_string(),
        trimmed => trimmed.to_string(),
    }
}

/// `Text <date time>.txt` for pasted text, `<host> <date>.url` (a
/// freedesktop link) for a pasted link: the name and the bytes to save.
pub(crate) fn pasted_text(text: &str, now: &gtk::glib::DateTime) -> (String, Vec<u8>) {
    let trimmed = text.trim();
    let link = (trimmed.starts_with("https://") || trimmed.starts_with("http://"))
        && !trimmed.contains(char::is_whitespace);
    if link {
        let host = link_host(trimmed);
        let date = now
            .format("%Y-%m-%d")
            .map(|d| d.to_string())
            .unwrap_or_default();
        let body = format!("[Desktop Entry]\nType=Link\nName={host}\nURL={trimmed}\n");
        (format!("{host} {date}.url"), body.into_bytes())
    } else {
        let stamp = now
            .format("%Y-%m-%d %H.%M.%S")
            .map(|d| d.to_string())
            .unwrap_or_default();
        (format!("Text {stamp}.txt"), text.as_bytes().to_vec())
    }
}
