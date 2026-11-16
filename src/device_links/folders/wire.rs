//! The `desklink.folders` packets: building them, sending them, and splitting
//! and reading manifests within the packet and part limits.
//!
//! Sibling edges: `model`, `state`. Outside: `packet`, `desk_features::send`.

use super::*;

pub(super) fn valid_pair_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

pub(super) fn folders_packet(op: &str, body: Value) -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_FOLDERS);
    packet.set("op", op);
    if let Value::Object(map) = body {
        packet.body.extend(map);
    }
    packet
}

pub(super) fn send(device: &str, packet: &NetworkPacket) {
    crate::device_links::desk_features::send(device, packet);
}

/// `manifest` packets for `pair`, each under [`MAX_PACKET_BYTES`].
pub(super) fn manifest_packets(id: &str, manifest: &Manifest) -> Vec<NetworkPacket> {
    let mut parts: Vec<Vec<Value>> = vec![Vec::new()];
    let mut size = 0;
    // Live entries first, then removals newest first: if the parts run out,
    // the oldest removals are what is left out.
    let mut ordered: Vec<(&String, &Entry)> = manifest.iter().collect();
    ordered.sort_by_key(|(_, entry)| (entry.deleted, std::cmp::Reverse(entry.mtime)));
    for (path, entry) in ordered.into_iter().take(MAX_FILES) {
        let mut value = serde_json::to_value(entry).unwrap_or(Value::Null);
        value["path"] = path.clone().into();
        let length = value.to_string().len() + 1;
        if size + length > MAX_PACKET_BYTES - 512 {
            parts.push(Vec::new());
            size = 0;
        }
        size += length;
        if let Some(part) = parts.last_mut() {
            part.push(value);
        }
    }
    let count = parts.len().min(MAX_PARTS);
    parts
        .into_iter()
        .take(MAX_PARTS)
        .enumerate()
        .map(|(index, entries)| {
            folders_packet(
                "manifest",
                json!({"id": id, "part": index + 1, "parts": count, "entries": entries}),
            )
        })
        .collect()
}

/// Entries of a `manifest` part that are safe to use, minus what this
/// pair ignores (a peer cannot have `.git/hooks/…` or hidden files written
/// here that the next scan would then make it delete).
pub(super) fn manifest_entries(packet: &NetworkPacket, pair: &Pair) -> Vec<(String, Entry)> {
    packet
        .body
        .get("entries")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .take(MAX_FILES)
                .filter_map(|value| {
                    let path = value.get("path")?.as_str()?;
                    let entry: Entry = serde_json::from_value(value.clone()).ok()?;
                    let sane = validate_relpath(path).is_ok()
                        && (!pair.bucket || bucket_path_ok(path, entry.dir))
                        && !ignored(path, pair.include_hidden, &pair.ignore)
                        && (entry.deleted
                            || entry.dir
                            || (entry.sha256.len() == 64
                                && entry.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                                && entry.size <= MAX_FILE_BYTES));
                    sane.then(|| {
                        (
                            path.to_string(),
                            Entry {
                                sha256: entry.sha256.to_ascii_lowercase(),
                                ..entry
                            },
                        )
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}
