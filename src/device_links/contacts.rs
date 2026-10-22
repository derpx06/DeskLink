//! The phone's contacts on this computer (DeskLink 1.2, F5): a cache the
//! phone keeps up to date, encrypted at rest, one per phone.
//!
//! The phone owns the contacts. On every feature-ready (and after a change)
//! it sends a complete snapshot of `uid → last-updated`; the desk asks for the
//! vCards of the new or changed uids and drops the uids the snapshot no
//! longer lists. `sharing: false` deletes that phone's cache, and so does
//! unpairing.
//!
//! On disk: `<config>/contacts/key` (32 random bytes, 0600) and one
//! `<sha256(device id)>.book` per phone: AES-256-GCM over the book's JSON,
//! a fresh nonce per write, the device id as associated data.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::packet::{
    NetworkPacket, PACKET_TYPE_CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS,
    PACKET_TYPE_CONTACTS_REQUEST_VCARDS_BY_UID,
};

/// More contacts than anyone keeps; the rest of a snapshot is ignored.
const MAX_CONTACTS: usize = 20_000;
/// uids asked for in one `request_vcards_by_uid` (K2).
const UIDS_PER_REQUEST: usize = 200;
const MAX_UID_BYTES: usize = 256;
const MAX_VCARD_BYTES: usize = 256 * 1024;
/// A ≤96 px JPEG is a few KiB; anything far larger is not a thumbnail.
const MAX_PHOTO_BYTES: usize = 64 * 1024;
/// Photos kept per phone in all (base64): the cache's disk and memory
/// bound; beyond it a contact is kept without its photo.
const MAX_PHOTOS_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_FIELD_CHARS: usize = 200;
const MAX_NOTE_CHARS: usize = 2_000;
const MAX_VALUES: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Phone {
    /// "Mobile", "Home", "Work", …; empty when the vCard gave none.
    pub(crate) kind: String,
    pub(crate) number: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Contact {
    pub(crate) uid: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) phones: Vec<Phone>,
    #[serde(default)]
    pub(crate) emails: Vec<String>,
    #[serde(default)]
    pub(crate) org: Option<String>,
    #[serde(default)]
    pub(crate) note: Option<String>,
    /// The JPEG thumbnail, base64.
    #[serde(default)]
    pub(crate) photo: Option<String>,
}

impl Contact {
    /// The photo's JPEG bytes.
    pub(crate) fn photo_bytes(&self) -> Option<Vec<u8>> {
        base64::engine::general_purpose::STANDARD
            .decode(self.photo.as_deref()?)
            .ok()
    }
}

/// One phone's cache.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Book {
    contacts: BTreeMap<String, Contact>,
    /// The phone's timestamp of each cached vCard.
    stamps: BTreeMap<String, String>,
    /// Asked for, not yet received: the timestamp the snapshot gave.
    #[serde(default)]
    wanted: BTreeMap<String, String>,
}

// ── Packets ────────────────────────────────────────────────────────────────

/// A K1 snapshot: `uid → timestamp` and whether the phone shares at all.
/// `None` when the packet is not one.
pub(crate) fn parse_snapshot(packet: &NetworkPacket) -> Option<(BTreeMap<String, String>, bool)> {
    let uids = packet.body.get("uids")?.as_array()?;
    let sharing = packet.get_bool("sharing").unwrap_or(true);
    let stamps = uids
        .iter()
        .filter_map(Value::as_str)
        .filter(|uid| valid_uid(uid))
        .take(MAX_CONTACTS)
        .map(|uid| {
            let stamp = match packet.body.get(uid) {
                Some(Value::String(stamp)) => stamp.chars().take(32).collect(),
                Some(Value::Number(stamp)) => stamp.to_string(),
                _ => String::new(),
            };
            (uid.to_string(), stamp)
        })
        .collect();
    Some((stamps, sharing))
}

fn valid_uid(uid: &str) -> bool {
    !uid.is_empty() && uid.len() <= MAX_UID_BYTES && !uid.chars().any(char::is_control)
}

/// The K2 requests for `uids`, at most [`UIDS_PER_REQUEST`] each.
fn vcard_requests(uids: &[String]) -> Vec<NetworkPacket> {
    uids.chunks(UIDS_PER_REQUEST)
        .map(|chunk| {
            let mut packet = NetworkPacket::new(PACKET_TYPE_CONTACTS_REQUEST_VCARDS_BY_UID);
            packet.set("uids", chunk.to_vec());
            packet
        })
        .collect()
}

/// Refresh: the phone answers with a snapshot.
pub(crate) fn refresh_request() -> NetworkPacket {
    NetworkPacket::new(PACKET_TYPE_CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS)
}

/// What a snapshot changes in `book`; the uids to ask for.
fn apply_snapshot(book: &mut Book, snapshot: BTreeMap<String, String>) -> Vec<String> {
    // Deletion by absence.
    book.contacts.retain(|uid, _| snapshot.contains_key(uid));
    book.stamps.retain(|uid, _| snapshot.contains_key(uid));
    book.wanted.clear();
    let mut ask = Vec::new();
    for (uid, stamp) in snapshot {
        let current = book.contacts.contains_key(&uid)
            && book.stamps.get(&uid).is_some_and(|known| *known == stamp);
        if !current {
            book.wanted.insert(uid.clone(), stamp);
            ask.push(uid);
        }
    }
    ask
}

/// A K3 reply into `book`: only uids it asked for. Whether anything changed.
fn apply_vcards(book: &mut Book, packet: &NetworkPacket) -> bool {
    let Some(uids) = packet.body.get("uids").and_then(Value::as_array) else {
        return false;
    };
    let mut changed = false;
    let mut photos: usize = book
        .contacts
        .values()
        .filter_map(|contact| contact.photo.as_ref().map(String::len))
        .sum();
    for uid in uids.iter().filter_map(Value::as_str) {
        let Some(stamp) = book.wanted.remove(uid) else {
            continue;
        };
        let Some(card) = packet
            .get_str(uid)
            .filter(|card| card.len() <= MAX_VCARD_BYTES)
        else {
            continue;
        };
        let Some(mut contact) = parse_vcard(uid, card) else {
            continue;
        };
        let replaced = book
            .contacts
            .get(uid)
            .and_then(|old| old.photo.as_ref().map(String::len))
            .unwrap_or(0);
        let photo = contact.photo.as_ref().map_or(0, String::len);
        if photos - replaced + photo > MAX_PHOTOS_TOTAL_BYTES {
            contact.photo = None;
        }
        photos = photos - replaced + contact.photo.as_ref().map_or(0, String::len);
        book.contacts.insert(uid.to_string(), contact);
        book.stamps.insert(uid.to_string(), stamp);
        changed = true;
    }
    changed
}

// ── vCard ──────────────────────────────────────────────────────────────────

/// The fields DeskLink shows from a vCard 3.0 (K3). `None` without a name
/// or anything to call or write to.
pub(crate) fn parse_vcard(uid: &str, card: &str) -> Option<Contact> {
    let mut contact = Contact {
        uid: uid.to_string(),
        name: String::new(),
        phones: Vec::new(),
        emails: Vec::new(),
        org: None,
        note: None,
        photo: None,
    };
    let mut structured_name = String::new();
    for line in unfold(card) {
        let Some((head, value)) = line.split_once(':') else {
            continue;
        };
        let mut parts = head.split(';');
        // Grouped properties ("item1.TEL") count as the property.
        let property = parts
            .next()
            .unwrap_or_default()
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        let params: Vec<String> = parts.map(str::to_ascii_uppercase).collect();
        match property.as_str() {
            "FN" => contact.name = clean(&unescape(value), MAX_FIELD_CHARS),
            "N" => {
                // Family;Given;Middle;Prefix;Suffix
                let fields: Vec<String> = split_unescaped(value, ';');
                let order = [3, 1, 2, 0, 4];
                structured_name = clean(
                    &order
                        .iter()
                        .filter_map(|&index| fields.get(index))
                        .filter(|field| !field.trim().is_empty())
                        .map(|field| field.trim())
                        .collect::<Vec<_>>()
                        .join(" "),
                    MAX_FIELD_CHARS,
                );
            }
            "TEL" if contact.phones.len() < MAX_VALUES => {
                let number = clean(&unescape(value), 40);
                if !number.is_empty() {
                    contact.phones.push(Phone {
                        kind: phone_kind(&params),
                        number,
                    });
                }
            }
            "EMAIL" if contact.emails.len() < MAX_VALUES => {
                let email = clean(&unescape(value), MAX_FIELD_CHARS);
                if !email.is_empty() {
                    contact.emails.push(email);
                }
            }
            "ORG" => {
                let org = clean(
                    &split_unescaped(value, ';')
                        .into_iter()
                        .filter(|part| !part.trim().is_empty())
                        .collect::<Vec<_>>()
                        .join(" · "),
                    MAX_FIELD_CHARS,
                );
                contact.org = Some(org).filter(|org| !org.is_empty());
            }
            "NOTE" => {
                let note: String = unescape(value)
                    .chars()
                    .filter(|c| *c == '\n' || !c.is_control())
                    .take(MAX_NOTE_CHARS)
                    .collect();
                contact.note = Some(note.trim().to_string()).filter(|note| !note.is_empty());
            }
            "PHOTO" => contact.photo = photo(&params, value),
            _ => {}
        }
    }
    if contact.name.is_empty() {
        contact.name = structured_name;
    }
    if contact.name.is_empty() {
        contact.name = contact
            .org
            .clone()
            .or_else(|| contact.phones.first().map(|phone| phone.number.clone()))
            .or_else(|| contact.emails.first().cloned())?;
    }
    Some(contact)
}

/// RFC 2425 §5.8.1: a line starting with a space or tab continues the last.
fn unfold(card: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in card.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        match (line.strip_prefix([' ', '\t']), lines.last_mut()) {
            (Some(rest), Some(last)) => last.push_str(rest),
            _ => lines.push(line.to_string()),
        }
    }
    lines
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// `value` split at unescaped `separator`s, each part unescaped.
fn split_unescaped(value: &str, separator: char) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut escaped = false;
    for c in value.chars() {
        if escaped {
            if let Some(part) = parts.last_mut() {
                part.push('\\');
                part.push(c);
            }
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == separator {
            parts.push(String::new());
        } else if let Some(part) = parts.last_mut() {
            part.push(c);
        }
    }
    parts.iter().map(|part| unescape(part)).collect()
}

fn clean(text: &str, max_chars: usize) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_string()
}

fn phone_kind(params: &[String]) -> String {
    let types = params
        .iter()
        .flat_map(|param| {
            let param = param.strip_prefix("TYPE=").unwrap_or(param);
            param.split(',').map(str::to_string).collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let has = |name: &str| types.iter().any(|kind| kind == name);
    if has("CELL") {
        "Mobile"
    } else if has("HOME") {
        "Home"
    } else if has("WORK") {
        "Work"
    } else if has("FAX") {
        "Fax"
    } else if has("PAGER") {
        "Pager"
    } else {
        ""
    }
    .to_string()
}

/// An inline base64 JPEG (`ENCODING=b`), checked and kept as base64.
fn photo(params: &[String], value: &str) -> Option<String> {
    let inline = params
        .iter()
        .any(|param| param == "ENCODING=B" || param == "ENCODING=BASE64");
    let data_uri = value.strip_prefix("data:image/jpeg;base64,");
    if !inline && data_uri.is_none() {
        return None;
    }
    let encoded: String = data_uri
        .unwrap_or(value)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&encoded)
        .ok()?;
    // A JPEG, and a small one.
    (bytes.len() <= MAX_PHOTO_BYTES && bytes.starts_with(&[0xFF, 0xD8, 0xFF]))
        .then(|| base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// The digits that identify a number across formats: the last nine, so
/// "+46 70 123 45 67" and "070-123 45 67" match (K4).
pub(crate) fn number_key(number: &str) -> Option<String> {
    let digits: String = number.chars().filter(char::is_ascii_digit).collect();
    (digits.len() >= 5).then(|| digits[digits.len().saturating_sub(9)..].to_string())
}

// ── The store ──────────────────────────────────────────────────────────────

struct Store {
    dir: PathBuf,
    books: HashMap<String, Book>,
}

fn store() -> &'static Mutex<Option<Store>> {
    static STORE: OnceLock<Mutex<Option<Store>>> = OnceLock::new();
    STORE.get_or_init(Default::default)
}

/// Bumped on every change, for the UI to redraw.
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Where the caches live: `<config dir>/contacts`. Called once at startup.
pub(crate) fn install(dir: PathBuf) {
    if let Ok(mut store) = store().lock() {
        *store = Some(Store {
            dir,
            books: HashMap::new(),
        });
    }
}

/// Runs `job` on `device_id`'s book (loaded from disk on first use).
fn with_book<T>(device_id: &str, job: impl FnOnce(&mut Book, &Path) -> T) -> Option<T> {
    let mut guard = store().lock().ok()?;
    let store = guard.as_mut()?;
    let dir = store.dir.clone();
    let book = store
        .books
        .entry(device_id.to_string())
        .or_insert_with(|| load(&dir, device_id).unwrap_or_default());
    Some(job(book, &dir))
}

/// A snapshot from `device_id` (K1): the requests to send.
pub(crate) fn on_snapshot(device_id: &str, packet: &NetworkPacket) -> Vec<NetworkPacket> {
    let Some((snapshot, sharing)) = parse_snapshot(packet) else {
        return Vec::new();
    };
    if !sharing {
        forget(device_id);
        return Vec::new();
    }
    let ask = with_book(device_id, |book, dir| {
        let before = book.contacts.len();
        let ask = apply_snapshot(book, snapshot);
        if book.contacts.len() != before {
            save_logged(dir, device_id, book);
        }
        ask
    })
    .unwrap_or_default();
    GENERATION.fetch_add(1, Ordering::Relaxed);
    debug_log!(
        "[Contacts] Snapshot from {device_id}: asking for {} vCards",
        ask.len()
    );
    vcard_requests(&ask)
}

/// vCards from `device_id` (K3).
pub(crate) fn on_vcards(device_id: &str, packet: &NetworkPacket) {
    let changed = with_book(device_id, |book, dir| {
        let changed = apply_vcards(book, packet);
        // ponytail: the whole book is rewritten per reply; batch the writes
        // if first syncs of very large address books feel slow.
        if changed {
            save_logged(dir, device_id, book);
        }
        changed
    })
    .unwrap_or(false);
    if changed {
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
}

/// Deletes `device_id`'s cache: the phone stopped sharing.
pub(crate) fn forget(device_id: &str) {
    let dir = store()
        .lock()
        .ok()
        .and_then(|store| store.as_ref().map(|store| store.dir.clone()));
    if let Some(dir) = dir {
        forget_in(&dir, device_id);
    }
}

/// Deletes `device_id`'s cache under `dir`, on disk and in memory:
/// unpaired, a new identity, or the phone stopped sharing.
pub(crate) fn forget_in(dir: &Path, device_id: &str) {
    if let Ok(mut store) = store().lock() {
        if let Some(store) = store.as_mut().filter(|store| store.dir == dir) {
            store.books.remove(device_id);
        }
    }
    let path = book_path(dir, device_id);
    if path.exists() {
        if let Err(error) = std::fs::remove_file(&path) {
            eprintln!("[Contacts] Could not delete {}: {error}", path.display());
        }
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// `device_id`'s contacts, by name.
pub(crate) fn list(device_id: &str) -> Vec<Contact> {
    let mut contacts: Vec<Contact> = with_book(device_id, |book, _| {
        book.contacts.values().cloned().collect()
    })
    .unwrap_or_default();
    contacts.sort_by_cached_key(|contact| contact.name.to_lowercase());
    contacts
}

pub(crate) fn by_uid(device_id: &str, uid: &str) -> Option<Contact> {
    with_book(device_id, |book, _| book.contacts.get(uid).cloned()).flatten()
}

/// The contact with `number` (last nine digits).
pub(crate) fn by_number(device_id: &str, number: &str) -> Option<Contact> {
    let key = number_key(number)?;
    with_book(device_id, |book, _| {
        book.contacts
            .values()
            .find(|contact| {
                contact
                    .phones
                    .iter()
                    .any(|phone| number_key(&phone.number).as_deref() == Some(key.as_str()))
            })
            .cloned()
    })
    .flatten()
}

/// The name a call's caller has in `device_id`'s contacts: by the phone's
/// `contactUid`, else by number.
pub(crate) fn caller_name(
    device_id: &str,
    uid: Option<&str>,
    number: Option<&str>,
) -> Option<String> {
    uid.and_then(|uid| by_uid(device_id, uid))
        .or_else(|| number.and_then(|number| by_number(device_id, number)))
        .map(|contact| contact.name)
}

// ── Encryption at rest ─────────────────────────────────────────────────────

fn book_path(dir: &Path, device_id: &str) -> PathBuf {
    let digest = openssl::sha::sha256(device_id.as_bytes());
    let name: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    dir.join(format!("{name}.book"))
}

fn save(dir: &Path, device_id: &str, book: &Book) -> Result<(), String> {
    let plain = serde_json::to_vec(book).map_err(|error| error.to_string())?;
    super::vault::seal(
        dir,
        &book_path(dir, device_id),
        device_id.as_bytes(),
        &plain,
    )
}

fn save_logged(dir: &Path, device_id: &str, book: &Book) {
    if let Err(error) = save(dir, device_id, book) {
        eprintln!("[Contacts] Could not save the contacts of {device_id}: {error}");
    }
}

fn load(dir: &Path, device_id: &str) -> Option<Book> {
    Some(super::vault::open_or_quarantine(
        dir,
        &book_path(dir, device_id),
        device_id.as_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn photos_stop_at_the_cache_bound_but_contacts_stay() {
        // A JPEG of about 60 KB, base64.
        let photo = format!("/9j/{}", "A".repeat(80_000));
        let mut book = Book::default();
        let count = MAX_PHOTOS_TOTAL_BYTES / photo.len() + 3;
        let mut packet = NetworkPacket::new("desklink.contacts.response_vcards");
        let uids: Vec<String> = (0..count).map(|index| format!("{index}")).collect();
        for uid in &uids {
            book.wanted.insert(uid.clone(), "1".into());
            packet.set(
                uid,
                format!("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:N {uid}\r\nTEL:555\r\nPHOTO;ENCODING=b;TYPE=JPEG:{photo}\r\nEND:VCARD\r\n"),
            );
        }
        packet.set("uids", uids.clone());
        assert!(apply_vcards(&mut book, &packet));
        assert_eq!(book.contacts.len(), count, "every contact is kept");
        let total: usize = book
            .contacts
            .values()
            .filter_map(|c| c.photo.as_ref())
            .map(String::len)
            .sum();
        assert!(
            total > MAX_PHOTOS_TOTAL_BYTES / 2 && total <= MAX_PHOTOS_TOTAL_BYTES,
            "{total}"
        );
        assert!(
            book.contacts.values().any(|c| c.photo.is_none()),
            "beyond it, without a photo"
        );
    }
    use serde_json::json;

    const CARD: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Lindqvist;Mara;;;\r\nFN:Mara Lindqvist\r\n\
TEL;TYPE=CELL:+46 70 123 45 67\r\nitem1.TEL;TYPE=WORK,VOICE:08-123 456\r\n\
EMAIL;TYPE=INTERNET:mara@example.org\r\nORG:Acme\\, Inc.;Ops\r\n\
NOTE:Line one\\nline two\r\nPHOTO;ENCODING=b;TYPE=JPEG:/9j/4\r\n AAQ\r\n\
X-UNKNOWN:ignored\r\nREV:2026-10-01T10:00:00Z\r\nEND:VCARD\r\n";

    #[test]
    fn a_vcard_gives_name_numbers_email_org_note_and_photo() {
        let contact = parse_vcard("u1", CARD).unwrap();
        assert_eq!(contact.name, "Mara Lindqvist");
        assert_eq!(
            contact.phones,
            vec![
                Phone {
                    kind: "Mobile".into(),
                    number: "+46 70 123 45 67".into()
                },
                Phone {
                    kind: "Work".into(),
                    number: "08-123 456".into()
                },
            ]
        );
        assert_eq!(contact.emails, vec!["mara@example.org".to_string()]);
        assert_eq!(contact.org.as_deref(), Some("Acme, Inc. · Ops"));
        assert_eq!(contact.note.as_deref(), Some("Line one\nline two"));
        // Folded across two lines, and a JPEG.
        assert_eq!(
            contact.photo_bytes().unwrap(),
            vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]
        );
    }

    #[test]
    fn a_vcard_without_fn_is_named_from_n_or_what_it_has() {
        let card = "BEGIN:VCARD\nN:Doe;Jane;Q;Dr.;\nEND:VCARD";
        assert_eq!(parse_vcard("u", card).unwrap().name, "Dr. Jane Q Doe");
        let card = "BEGIN:VCARD\nTEL:555 0100\nEND:VCARD";
        assert_eq!(parse_vcard("u", card).unwrap().name, "555 0100");
        assert!(parse_vcard("u", "BEGIN:VCARD\nEND:VCARD").is_none());
        // Not a JPEG: no photo.
        let card = "BEGIN:VCARD\nFN:A\nPHOTO;ENCODING=b;TYPE=PNG:iVBORw0KGgo=\nEND:VCARD";
        assert!(parse_vcard("u", card).unwrap().photo.is_none());
    }

    fn snapshot(entries: &[(&str, &str)], sharing: bool) -> NetworkPacket {
        let mut packet = NetworkPacket::new("desklink.contacts.response_uids_timestamps");
        packet.set(
            "uids",
            entries
                .iter()
                .map(|(uid, _)| uid.to_string())
                .collect::<Vec<_>>(),
        );
        for (uid, stamp) in entries {
            packet.set(uid, *stamp);
        }
        packet.set("sharing", sharing);
        packet
    }

    fn vcards(entries: &[(&str, &str)]) -> NetworkPacket {
        let mut packet = NetworkPacket::new("desklink.contacts.response_vcards");
        packet.set(
            "uids",
            entries
                .iter()
                .map(|(uid, _)| uid.to_string())
                .collect::<Vec<_>>(),
        );
        for (uid, name) in entries {
            packet.set(uid, format!("BEGIN:VCARD\nFN:{name}\nEND:VCARD"));
        }
        packet
    }

    #[test]
    fn a_snapshot_asks_for_what_changed_and_drops_what_is_gone() {
        let mut book = Book::default();
        let (first, sharing) = parse_snapshot(&snapshot(&[("a", "1"), ("b", "1")], true)).unwrap();
        assert!(sharing);
        assert_eq!(apply_snapshot(&mut book, first), vec!["a", "b"]);
        assert!(apply_vcards(
            &mut book,
            &vcards(&[("a", "Ann"), ("b", "Bo")])
        ));
        // Unasked uids are ignored.
        assert!(!apply_vcards(&mut book, &vcards(&[("z", "Zed")])));
        assert_eq!(book.contacts.len(), 2);

        // b changed, a is gone, c is new.
        let (next, _) = parse_snapshot(&snapshot(&[("b", "2"), ("c", "1")], true)).unwrap();
        assert_eq!(apply_snapshot(&mut book, next), vec!["b", "c"]);
        assert!(!book.contacts.contains_key("a"), "deleted by absence");
        assert_eq!(
            book.contacts["b"].name, "Bo",
            "kept until the new card comes"
        );
        // The reply is split: c first, b later.
        assert!(apply_vcards(&mut book, &vcards(&[("c", "Cy")])));
        assert!(apply_vcards(&mut book, &vcards(&[("b", "Bea")])));
        assert_eq!(book.contacts["b"].name, "Bea");

        // Nothing changed: nothing asked.
        let (same, _) = parse_snapshot(&snapshot(&[("b", "2"), ("c", "1")], true)).unwrap();
        assert!(apply_snapshot(&mut book, same).is_empty());
    }

    #[test]
    fn requests_carry_at_most_two_hundred_uids() {
        let uids: Vec<String> = (0..450).map(|n| n.to_string()).collect();
        let requests = vcard_requests(&uids);
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].body["uids"].as_array().unwrap().len(), 200);
        assert_eq!(requests[2].body["uids"].as_array().unwrap().len(), 50);
        assert_eq!(
            requests[0].packet_type,
            "desklink.contacts.request_vcards_by_uid"
        );
        // Not-a-snapshot and junk uids.
        let mut junk = NetworkPacket::new("desklink.contacts.response_uids_timestamps");
        junk.body
            .insert("uids".into(), json!(["", "ok", 5, "a\u{1}b"]));
        let (stamps, _) = parse_snapshot(&junk).unwrap();
        assert_eq!(stamps.keys().collect::<Vec<_>>(), vec!["ok"]);
        assert!(parse_snapshot(&NetworkPacket::new("x")).is_none());
    }

    #[test]
    fn numbers_match_on_their_last_nine_digits() {
        assert_eq!(number_key("+46 70 123 45 67"), number_key("070-123 45 67"));
        assert_ne!(
            number_key("+46 70 123 45 67"),
            number_key("+46 70 123 45 68")
        );
        assert_eq!(number_key("112"), None, "too short to identify anyone");
    }

    #[test]
    fn the_cache_is_encrypted_private_and_bound_to_its_phone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("desklink-contacts-{}", uuid::Uuid::new_v4()));
        let mut book = Book::default();
        book.contacts.insert(
            "a".into(),
            parse_vcard("a", "FN:Secret Name\nTEL:555 0100").unwrap(),
        );
        save(&dir, "phone-1", &book).unwrap();
        let path = book_path(&dir, "phone-1");
        let raw = std::fs::read(&path).unwrap();
        assert!(
            !String::from_utf8_lossy(&raw).contains("Secret"),
            "not plain text"
        );
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir.join("key")), 0o600);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(
            load(&dir, "phone-1").unwrap().contacts["a"].name,
            "Secret Name"
        );
        // Another phone's id cannot open it, even renamed into place.
        std::fs::copy(&path, book_path(&dir, "phone-2")).unwrap();
        assert!(load(&dir, "phone-2").unwrap().contacts.is_empty());
        assert!(
            !book_path(&dir, "phone-2").exists(),
            "set aside, not read as another phone's"
        );
        forget_in(&dir, "phone-1");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
