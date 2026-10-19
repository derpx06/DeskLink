//! Desktop client for the phone's `desklink.file.browser.v1` protocol.
//!
//! Android answers these requests from a fixed set of authorised storage roots
//! and hands out random entry IDs, so no phone path or URI ever crosses the
//! wire. This module builds the requests and validates what comes back; it has
//! no I/O and no GTK, so all of it is testable without a phone.
//!
//! A reply is a *report*, not an instruction. It is only accepted when it
//! answers a request this desktop issued and is still waiting on, and it is
//! parsed by the shape that request expects rather than by which keys happen to
//! be present.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use uuid::Uuid;

use super::{WebRtcChannel, WebRtcEnvelope, WebRtcWireBinding};
use crate::device_links::core::{DeviceManager, FeatureTransportSnapshot, SessionBinding};

pub const FILE_BROWSER_MESSAGE_TYPE: &str = "desklink.file.browser.v1";

const BROWSER_VERSION: u64 = 1;
const MAX_ID_LEN: usize = 128;
pub const MAX_NAME_LEN: usize = 255;
/// Android pages at 256 so one reply stays under the 128 KiB envelope limit.
const MAX_ENTRIES: usize = 256;
const MAX_ERROR_LEN: usize = 200;
/// Bounds the table an unresponsive phone could otherwise grow.
pub const MAX_PENDING_REQUESTS: usize = 32;
/// Longer than any healthy LAN round trip; short enough that the UI is not
/// left spinning on a phone that has gone away.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserRequest {
    Roots,
    /// One page of a folder. `offset` is where the phone's previous page said to
    /// continue (`next_offset`); 0 starts the listing.
    List {
        entry_id: String,
        offset: u32,
    },
    Metadata {
        entry_id: String,
    },
    /// `parent_id` is the folder the new one is created in.
    CreateFolder {
        parent_id: String,
        name: String,
    },
    Rename {
        entry_id: String,
        name: String,
    },
    Move {
        entry_id: String,
        destination_id: String,
    },
    Delete {
        entry_id: String,
    },
    Download {
        entry_id: String,
        transfer_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserEntry {
    pub entry_id: String,
    pub root_id: String,
    /// Absent for a storage root.
    pub parent_id: Option<String>,
    pub name: String,
    pub directory: bool,
    pub size: u64,
    /// Milliseconds since the Unix epoch; 0 when the phone does not know.
    pub modified: u64,
    pub mime_type: Option<String>,
    pub readable: bool,
    pub writable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserResult {
    /// Reply to `Roots` (`parent_id` is `None`) and `List`.
    Entries {
        parent_id: Option<String>,
        entries: Vec<BrowserEntry>,
        /// Where to ask for the rest of a folder that did not fit one reply.
        next_offset: Option<u32>,
    },
    /// Reply to `Metadata`, `CreateFolder`, `Rename` and `Move`.
    Entry(BrowserEntry),
    Deleted {
        entry_id: String,
    },
    /// The phone has agreed to send the file; the bytes follow as an ordinary
    /// checkpointed transfer carrying the same `transfer_id`.
    Download {
        transfer_id: String,
        name: String,
        size: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserResponse {
    pub request_id: String,
    pub result: Result<BrowserResult, String>,
}

impl BrowserRequest {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Roots => Ok(()),
            Self::List { entry_id, .. }
            | Self::Metadata { entry_id }
            | Self::Delete { entry_id } => validate_id(entry_id),
            Self::CreateFolder { parent_id, name } => {
                validate_id(parent_id)?;
                validate_name(name)
            }
            Self::Rename { entry_id, name } => {
                validate_id(entry_id)?;
                validate_name(name)
            }
            Self::Move {
                entry_id,
                destination_id,
            } => {
                validate_id(entry_id)?;
                validate_id(destination_id)
            }
            Self::Download {
                entry_id,
                transfer_id,
            } => {
                validate_id(entry_id)?;
                validate_transfer_id(transfer_id)
            }
        }
    }

    fn to_json(&self, request_id: &str) -> Result<Vec<u8>, String> {
        let mut body = json!({
            "browserVersion": BROWSER_VERSION,
            "requestId": request_id,
        });
        let fields = body.as_object_mut().expect("json! built an object");
        let mut put = |key: &str, value: &str| {
            fields.insert(key.to_string(), Value::String(value.to_string()));
        };
        match self {
            Self::Roots => put("action", "roots"),
            Self::List { entry_id, .. } => {
                put("action", "list");
                put("entryId", entry_id);
            }
            Self::Metadata { entry_id } => {
                put("action", "metadata");
                put("entryId", entry_id);
            }
            // Android reads the *parent* folder from `entryId` for this action.
            Self::CreateFolder { parent_id, name } => {
                put("action", "create-folder");
                put("entryId", parent_id);
                put("name", name.trim());
            }
            Self::Rename { entry_id, name } => {
                put("action", "rename");
                put("entryId", entry_id);
                put("name", name.trim());
            }
            Self::Move {
                entry_id,
                destination_id,
            } => {
                put("action", "move");
                put("entryId", entry_id);
                put("destinationId", destination_id);
            }
            Self::Delete { entry_id } => {
                put("action", "delete");
                put("entryId", entry_id);
            }
            Self::Download {
                entry_id,
                transfer_id,
            } => {
                put("action", "download");
                put("entryId", entry_id);
                put("transferId", transfer_id);
            }
        }
        // A first page omits it, so a phone that predates paging is unaffected.
        if let Self::List { offset, .. } = self {
            if *offset > 0 {
                body["offset"] = json!(offset);
            }
        }
        serde_json::to_vec(&body).map_err(|error| error.to_string())
    }

    fn expected(&self) -> Expected {
        match self {
            Self::Roots => Expected::Entries { offset: 0 },
            Self::List { offset, .. } => Expected::Entries { offset: *offset },
            Self::Metadata { .. }
            | Self::CreateFolder { .. }
            | Self::Rename { .. }
            | Self::Move { .. } => Expected::Entry,
            Self::Delete { entry_id } => Expected::Deleted(entry_id.clone()),
            Self::Download { transfer_id, .. } => Expected::Download(transfer_id.clone()),
        }
    }
}

/// What a reply must look like, fixed when the request is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Expected {
    /// `offset` is the page asked for; a reply must confirm it.
    Entries {
        offset: u32,
    },
    Entry,
    Deleted(String),
    Download(String),
}

/// The requests still waiting for the phone.
#[derive(Debug, Default)]
pub struct PendingBrowserRequests {
    open: HashMap<String, (Expected, Instant)>,
}

impl PendingBrowserRequests {
    /// Validates `request`, records it, and returns its ID and wire bytes.
    pub fn begin(
        &mut self,
        request: &BrowserRequest,
        now: Instant,
    ) -> Result<(String, Vec<u8>), String> {
        request.validate()?;
        self.expire(now);
        if self.open.len() >= MAX_PENDING_REQUESTS {
            return Err("Too many phone-file requests are waiting for a reply".to_string());
        }
        let request_id = Uuid::new_v4().to_string();
        let bytes = request.to_json(&request_id)?;
        self.open
            .insert(request_id.clone(), (request.expected(), now));
        Ok((request_id, bytes))
    }

    /// Drops requests the phone never answered, returning their IDs so the UI
    /// can stop waiting on them.
    pub fn expire(&mut self, now: Instant) -> Vec<String> {
        let stale: Vec<String> = self
            .open
            .iter()
            .filter(|(_, (_, sent))| now.saturating_duration_since(*sent) >= REQUEST_TIMEOUT)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &stale {
            self.open.remove(id);
        }
        stale
    }

    /// Forgets everything, e.g. when the WebRTC session is replaced: a reply on
    /// a new session can never answer a question asked on the old one.
    pub fn clear(&mut self) -> Vec<String> {
        self.open.drain().map(|(id, _)| id).collect()
    }

    pub fn len(&self) -> usize {
        self.open.len()
    }

    /// Matches a reply to its request and validates it.
    ///
    /// A reply that answers nothing we asked is an error and changes no state,
    /// so a phone (or anything else on the channel) cannot push entries into the
    /// UI unprompted.
    pub fn resolve(&mut self, payload: &[u8]) -> Result<BrowserResponse, String> {
        let value: Value = serde_json::from_slice(payload)
            .map_err(|_| "Malformed phone-file reply".to_string())?;
        if value.get("browserVersion").and_then(Value::as_u64) != Some(BROWSER_VERSION) {
            return Err("Unsupported phone-file protocol version".to_string());
        }
        let request_id = value
            .get("requestId")
            .and_then(Value::as_str)
            .ok_or_else(|| "Phone-file reply has no request ID".to_string())?
            .to_string();
        let Some((expected, _)) = self.open.remove(&request_id) else {
            return Err("Phone-file reply does not answer an open request".to_string());
        };
        let ok = value.get("ok").and_then(Value::as_bool);
        let result = match ok {
            Some(true) => parse_result(&expected, value.get("result")),
            Some(false) => Err(clean_text(
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Phone-file operation failed"),
                MAX_ERROR_LEN,
            )),
            None => Err("Phone-file reply has no status".to_string()),
        };
        Ok(BrowserResponse { request_id, result })
    }
}

/// Browser state per paired device, shared by the WebRTC receive path (which
/// resolves replies) and the GTK thread (which starts requests and drains
/// replies). One short lock each side; neither ever waits on the network.
#[derive(Default)]
struct DeviceBrowser {
    pending: PendingBrowserRequests,
    replies: Vec<BrowserResponse>,
}

fn browsers() -> &'static std::sync::Mutex<HashMap<String, DeviceBrowser>> {
    static BROWSERS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, DeviceBrowser>>> =
        std::sync::OnceLock::new();
    BROWSERS.get_or_init(Default::default)
}

fn with_browser<T>(device_id: &str, action: impl FnOnce(&mut DeviceBrowser) -> T) -> Option<T> {
    let mut all = browsers().lock().ok()?;
    Some(action(all.entry(device_id.to_string()).or_default()))
}

/// Sends a request on the WebRTC file-control channel.
pub(crate) fn send_request(
    snapshot: &FeatureTransportSnapshot,
    payload: &[u8],
) -> Result<(), String> {
    let peer = snapshot
        .web_rtc_peer
        .as_ref()
        .ok_or_else(|| "DeskLink WebRTC handover has no active peer".to_string())?;
    let wire = snapshot
        .web_rtc_wire
        .as_ref()
        .ok_or_else(|| "DeskLink WebRTC handover has no wire binding".to_string())?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default();
    let envelope = WebRtcEnvelope::create(
        wire,
        WebRtcChannel::FileControl,
        FILE_BROWSER_MESSAGE_TYPE,
        payload,
        millis,
    )?;
    let text = String::from_utf8(envelope.to_json()?)
        .map_err(|_| "DeskLink WebRTC file envelope was not UTF-8".to_string())?;
    peer.send_text(WebRtcChannel::FileControl, &text)
}

/// Handles an inbound `desklink.file.browser.v1` envelope: the same session,
/// channel and replay checks every file message gets, then the reply is matched
/// to the request it answers.
pub(crate) fn handle_envelope(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    channel: WebRtcChannel,
    envelope: WebRtcEnvelope,
    sessions: &DeviceManager,
) -> Result<(), String> {
    if !sessions
        .webrtc_features_ready(binding, attempt_id)
        .map_err(|error| error.to_string())?
    {
        return Err("Phone-file reply arrived before feature handover".to_string());
    }
    if channel != WebRtcChannel::FileControl
        || envelope.channel != WebRtcChannel::FileControl.label()
    {
        return Err("Phone-file reply arrived on the wrong channel".to_string());
    }
    let payload = envelope.validate(wire)?;
    sessions
        .accept_webrtc_envelope(binding, attempt_id, envelope.message_id)
        .map_err(|error| error.to_string())?;
    accept_reply(&binding.device_id, &payload)
}

/// Registers a request for `device_id`, returning its ID and the bytes to send.
pub(crate) fn begin_request(
    device_id: &str,
    request: &BrowserRequest,
) -> Result<(String, Vec<u8>), String> {
    with_browser(device_id, |browser| {
        browser.pending.begin(request, Instant::now())
    })
    .unwrap_or_else(|| Err("Phone-file state is unavailable".to_string()))
}

/// Applies a reply that arrived on the file-control channel.
pub(crate) fn accept_reply(device_id: &str, payload: &[u8]) -> Result<(), String> {
    with_browser(device_id, |browser| {
        let response = browser.pending.resolve(payload)?;
        browser.replies.push(response);
        Ok(())
    })
    .unwrap_or_else(|| Err("Phone-file state is unavailable".to_string()))
}

/// Gives up on a request that could not be sent, so the UI hears about it now
/// instead of after the timeout.
pub(crate) fn fail_request(device_id: &str, request_id: &str, error: &str) {
    with_browser(device_id, |browser| {
        if browser.pending.open.remove(request_id).is_some() {
            browser.replies.push(BrowserResponse {
                request_id: request_id.to_string(),
                result: Err(clean_text(error, MAX_ERROR_LEN)),
            });
        }
    });
}

/// Ends every open request because the session they were asked on is gone.
pub(crate) fn reset(device_id: &str, reason: &str) {
    with_browser(device_id, |browser| {
        for request_id in browser.pending.clear() {
            browser.replies.push(BrowserResponse {
                request_id,
                result: Err(reason.to_string()),
            });
        }
    });
}

/// Replies (including timeouts) that the UI has not seen yet.
pub fn take_replies(device_id: &str) -> Vec<BrowserResponse> {
    with_browser(device_id, |browser| {
        for request_id in browser.pending.expire(Instant::now()) {
            browser.replies.push(BrowserResponse {
                request_id,
                result: Err("The phone did not answer in time".to_string()),
            });
        }
        std::mem::take(&mut browser.replies)
    })
    .unwrap_or_default()
}

fn parse_result(expected: &Expected, result: Option<&Value>) -> Result<BrowserResult, String> {
    let result = result
        .and_then(Value::as_object)
        .ok_or_else(|| "Phone-file reply has no result".to_string())?;
    match expected {
        Expected::Entries { offset } => {
            let list = result
                .get("entries")
                .and_then(Value::as_array)
                .ok_or_else(|| "Phone-file listing has no entries".to_string())?;
            if list.len() > MAX_ENTRIES {
                return Err("Phone-file listing is too large".to_string());
            }
            let parent_id = match result.get("parentId") {
                None | Some(Value::Null) => None,
                Some(Value::String(id)) => Some(checked_id(id)?),
                Some(_) => return Err("Phone-file listing has an invalid parent".to_string()),
            };
            let entries = list
                .iter()
                .map(parse_entry)
                .collect::<Result<Vec<_>, _>>()?;
            // A page after the first is only meaningful if the phone says which
            // page it is; otherwise an old phone would append its first page again.
            if *offset > 0
                && result.get("offset").and_then(Value::as_u64) != Some(u64::from(*offset))
            {
                return Err("The phone did not continue the listing".to_string());
            }
            let next_offset = match result.get("nextOffset") {
                None | Some(Value::Null) => None,
                Some(value) => Some(
                    value
                        .as_u64()
                        .and_then(|next| u32::try_from(next).ok())
                        .filter(|next| next > offset)
                        .ok_or_else(|| "Phone-file listing has an invalid next page".to_string())?,
                ),
            };
            Ok(BrowserResult::Entries {
                parent_id,
                entries,
                next_offset,
            })
        }
        Expected::Entry => {
            let entry = result
                .get("entry")
                .ok_or_else(|| "Phone-file reply has no entry".to_string())?;
            Ok(BrowserResult::Entry(parse_entry(entry)?))
        }
        Expected::Deleted(asked) => {
            let deleted = result
                .get("deletedEntryId")
                .and_then(Value::as_str)
                .ok_or_else(|| "Phone-file delete reply has no entry ID".to_string())?;
            if deleted != asked {
                return Err("Phone-file delete reply names a different entry".to_string());
            }
            Ok(BrowserResult::Deleted {
                entry_id: deleted.to_string(),
            })
        }
        Expected::Download(asked) => {
            let transfer_id = result
                .get("transferId")
                .and_then(Value::as_str)
                .ok_or_else(|| "Phone-file download reply has no transfer ID".to_string())?;
            if transfer_id != asked {
                return Err("Phone-file download reply names a different transfer".to_string());
            }
            Ok(BrowserResult::Download {
                transfer_id: transfer_id.to_string(),
                name: clean_name(result.get("name").and_then(Value::as_str).unwrap_or("")),
                size: result.get("size").and_then(Value::as_u64).unwrap_or(0),
            })
        }
    }
}

fn parse_entry(value: &Value) -> Result<BrowserEntry, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "Phone-file entry is not an object".to_string())?;
    let text = |key: &str| object.get(key).and_then(Value::as_str);
    let flag = |key: &str| object.get(key).and_then(Value::as_bool).unwrap_or(false);
    Ok(BrowserEntry {
        entry_id: checked_id(text("entryId").ok_or("Phone-file entry has no ID")?)?,
        root_id: checked_id(text("rootId").ok_or("Phone-file entry has no root")?)?,
        // Android omits the key for a root, because `JSONObject.put(key, null)`
        // removes it.
        parent_id: match object.get("parentId") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) => Some(checked_id(id)?),
            Some(_) => return Err("Phone-file entry has an invalid parent".to_string()),
        },
        name: clean_name(text("name").unwrap_or("")),
        directory: flag("directory"),
        size: object.get("size").and_then(Value::as_u64).unwrap_or(0),
        modified: object.get("modified").and_then(Value::as_u64).unwrap_or(0),
        mime_type: text("mimeType").map(|mime| clean_text(mime, 100)),
        readable: flag("readable"),
        writable: flag("writable"),
    })
}

fn checked_id(id: &str) -> Result<String, String> {
    validate_id(id)?;
    Ok(id.to_string())
}

fn validate_id(id: &str) -> Result<(), String> {
    if (1..=MAX_ID_LEN).contains(&id.len()) {
        Ok(())
    } else {
        Err("Invalid phone-file entry ID".to_string())
    }
}

/// The same rule Android applies, so a name is refused here before it costs a
/// round trip.
pub fn validate_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_NAME_LEN
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', '\0'])
    {
        return Err("Invalid file or folder name".to_string());
    }
    Ok(())
}

fn validate_transfer_id(id: &str) -> Result<(), String> {
    if (1..=MAX_ID_LEN).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(())
    } else {
        Err("Invalid phone-file transfer ID".to_string())
    }
}

/// A fresh transfer ID in the alphabet Android accepts.
pub fn new_transfer_id() -> String {
    Uuid::new_v4().to_string()
}

/// Names come from the phone and are shown to the user, so control characters
/// (terminal escapes, newlines, bidi tricks in a label) are removed.
fn clean_text(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect()
}

fn clean_name(name: &str) -> String {
    let cleaned = clean_text(name, MAX_NAME_LEN);
    if cleaned.trim().is_empty() {
        "Unnamed".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    fn reply(request_id: &str, body: Value) -> Vec<u8> {
        let mut base = json!({"browserVersion": 1, "requestId": request_id});
        base.as_object_mut()
            .unwrap()
            .extend(body.as_object().unwrap().clone());
        serde_json::to_vec(&base).unwrap()
    }

    fn entry_json(id: &str, name: &str, directory: bool) -> Value {
        json!({
            "entryId": id, "rootId": "root-1", "parentId": "root-1", "name": name,
            "directory": directory, "size": 12, "modified": 1_700_000_000_000u64,
            "mimeType": null, "readable": true, "writable": true
        })
    }

    fn sent(pending: &mut PendingBrowserRequests, request: BrowserRequest) -> (String, Value) {
        let (id, bytes) = pending.begin(&request, now()).unwrap();
        (id, serde_json::from_slice(&bytes).unwrap())
    }

    /// Field names are the contract with `WebRtcPhoneFileBrowser.kt`. Android
    /// reads a create-folder's *parent* from `entryId`; getting that wrong
    /// would create the folder in the wrong place or not at all.
    #[test]
    fn requests_use_the_field_names_android_reads() {
        let mut pending = PendingBrowserRequests::default();

        let (id, body) = sent(&mut pending, BrowserRequest::Roots);
        assert_eq!(body["action"], "roots");
        assert_eq!(body["browserVersion"], 1);
        assert_eq!(body["requestId"], id.as_str());

        let (_, body) = sent(
            &mut pending,
            BrowserRequest::CreateFolder {
                parent_id: "parent".into(),
                name: "  Trips ".into(),
            },
        );
        assert_eq!(body["action"], "create-folder");
        assert_eq!(body["entryId"], "parent");
        assert_eq!(body["name"], "Trips");

        let (_, body) = sent(
            &mut pending,
            BrowserRequest::Move {
                entry_id: "a".into(),
                destination_id: "b".into(),
            },
        );
        assert_eq!(body["action"], "move");
        assert_eq!(body["entryId"], "a");
        assert_eq!(body["destinationId"], "b");

        let (_, body) = sent(
            &mut pending,
            BrowserRequest::Download {
                entry_id: "a".into(),
                transfer_id: "t-1".into(),
            },
        );
        assert_eq!(body["action"], "download");
        assert_eq!(body["transferId"], "t-1");
    }

    #[test]
    fn unsafe_names_and_ids_are_refused_before_sending() {
        let mut pending = PendingBrowserRequests::default();
        for name in ["", "  ", ".", "..", "a/b", "a\\b", "nul\0"] {
            let request = BrowserRequest::Rename {
                entry_id: "a".into(),
                name: name.into(),
            };
            assert!(pending.begin(&request, now()).is_err(), "{name:?}");
        }
        let long = "x".repeat(MAX_NAME_LEN + 1);
        assert!(pending
            .begin(
                &BrowserRequest::CreateFolder {
                    parent_id: "p".into(),
                    name: long
                },
                now()
            )
            .is_err());
        assert!(pending
            .begin(
                &BrowserRequest::List {
                    entry_id: String::new(),
                    offset: 0
                },
                now()
            )
            .is_err());
        assert!(pending
            .begin(
                &BrowserRequest::Download {
                    entry_id: "a".into(),
                    transfer_id: "../etc".into()
                },
                now()
            )
            .is_err());
        assert_eq!(pending.len(), 0, "a refused request must not be tracked");
    }

    #[test]
    fn a_listing_is_parsed_and_display_names_are_cleaned() {
        let mut pending = PendingBrowserRequests::default();
        let (id, _) = sent(
            &mut pending,
            BrowserRequest::List {
                entry_id: "root-1".into(),
                offset: 0,
            },
        );
        let response = pending
            .resolve(&reply(
                &id,
                json!({"ok": true, "result": {
                    "parentId": "root-1",
                    "entries": [
                        entry_json("e1", "Photos", true),
                        entry_json("e2", "evil\u{1b}[31m\nname.txt", false),
                    ]
                }}),
            ))
            .unwrap();

        let Ok(BrowserResult::Entries {
            parent_id, entries, ..
        }) = response.result
        else {
            panic!("expected a listing");
        };
        assert_eq!(parent_id.as_deref(), Some("root-1"));
        assert_eq!(entries.len(), 2);
        assert!(entries[0].directory);
        assert_eq!(entries[1].name, "evil[31mname.txt");
        assert_eq!(entries[1].mime_type, None);
        assert_eq!(pending.len(), 0, "an answered request is forgotten");
    }

    #[test]
    fn a_root_entry_without_a_parent_key_is_accepted() {
        let mut pending = PendingBrowserRequests::default();
        let (id, _) = sent(&mut pending, BrowserRequest::Roots);
        let mut root = entry_json("r1", "Internal storage", true);
        root.as_object_mut().unwrap().remove("parentId");

        let response = pending
            .resolve(&reply(
                &id,
                json!({"ok": true, "result": {"entries": [root]}}),
            ))
            .unwrap();

        let Ok(BrowserResult::Entries {
            parent_id, entries, ..
        }) = response.result
        else {
            panic!("expected roots");
        };
        assert_eq!(parent_id, None);
        assert_eq!(entries[0].parent_id, None);
    }

    #[test]
    fn a_reply_that_answers_no_open_request_is_rejected() {
        let mut pending = PendingBrowserRequests::default();
        let unsolicited = reply("not-ours", json!({"ok": true, "result": {"entries": []}}));
        assert!(pending.resolve(&unsolicited).is_err());

        // ...and a reply can answer a request only once.
        let (id, _) = sent(&mut pending, BrowserRequest::Roots);
        let body = reply(&id, json!({"ok": true, "result": {"entries": []}}));
        assert!(pending.resolve(&body).is_ok());
        assert!(pending.resolve(&body).is_err());
    }

    #[test]
    fn the_shape_is_decided_by_the_request_not_by_the_reply() {
        let mut pending = PendingBrowserRequests::default();
        let (id, _) = sent(
            &mut pending,
            BrowserRequest::Delete {
                entry_id: "asked".into(),
            },
        );
        // A reply carrying a listing must not satisfy a delete...
        let response = pending
            .resolve(&reply(&id, json!({"ok": true, "result": {"entries": []}})))
            .unwrap();
        assert!(response.result.is_err());

        // ...and one naming a different entry is not a confirmation either.
        let (id, _) = sent(
            &mut pending,
            BrowserRequest::Delete {
                entry_id: "asked".into(),
            },
        );
        let response = pending
            .resolve(&reply(
                &id,
                json!({"ok": true, "result": {"deletedEntryId": "other"}}),
            ))
            .unwrap();
        assert!(response.result.is_err());
    }

    #[test]
    fn a_download_reply_must_carry_the_transfer_id_we_chose() {
        let mut pending = PendingBrowserRequests::default();
        let request = BrowserRequest::Download {
            entry_id: "e".into(),
            transfer_id: "mine".into(),
        };
        let (id, _) = sent(&mut pending, request.clone());
        let bad = pending
            .resolve(&reply(
                &id,
                json!({"ok": true, "result": {"transferId": "theirs", "name": "a", "size": 1}}),
            ))
            .unwrap();
        assert!(bad.result.is_err());

        let (id, _) = sent(&mut pending, request);
        let good = pending
            .resolve(&reply(
                &id,
                json!({"ok": true, "result": {"transferId": "mine", "name": "a.bin", "size": 42}}),
            ))
            .unwrap();
        assert_eq!(
            good.result,
            Ok(BrowserResult::Download {
                transfer_id: "mine".into(),
                name: "a.bin".into(),
                size: 42
            })
        );
    }

    #[test]
    fn a_failure_reply_surfaces_the_phones_message() {
        let mut pending = PendingBrowserRequests::default();
        let (id, _) = sent(&mut pending, BrowserRequest::Roots);
        let response = pending
            .resolve(&reply(
                &id,
                json!({"ok": false, "error": "Storage root is unavailable"}),
            ))
            .unwrap();
        assert_eq!(response.result, Err("Storage root is unavailable".into()));
    }

    fn listing_page(
        pending: &mut PendingBrowserRequests,
        offset: u32,
        result: Value,
    ) -> BrowserResponse {
        let (id, _) = sent(
            pending,
            BrowserRequest::List {
                entry_id: "root-1".into(),
                offset,
            },
        );
        pending
            .resolve(&reply(&id, json!({"ok": true, "result": result})))
            .unwrap()
    }

    #[test]
    fn a_later_page_is_requested_by_offset_and_a_first_page_omits_it() {
        let mut pending = PendingBrowserRequests::default();
        let (_, first) = sent(
            &mut pending,
            BrowserRequest::List {
                entry_id: "f".into(),
                offset: 0,
            },
        );
        assert!(
            first.get("offset").is_none(),
            "old phones never saw an offset"
        );

        let (_, next) = sent(
            &mut pending,
            BrowserRequest::List {
                entry_id: "f".into(),
                offset: 170,
            },
        );
        assert_eq!(next["action"], "list");
        assert_eq!(next["entryId"], "f");
        assert_eq!(next["offset"], 170);
    }

    #[test]
    fn a_partial_listing_says_where_to_continue() {
        let mut pending = PendingBrowserRequests::default();
        let response = listing_page(
            &mut pending,
            0,
            json!({"parentId": "f", "offset": 0, "nextOffset": 2,
                   "entries": [entry_json("a", "a", false), entry_json("b", "b", false)]}),
        );
        let Ok(BrowserResult::Entries {
            next_offset,
            entries,
            ..
        }) = response.result
        else {
            panic!("expected entries: {response:?}");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(next_offset, Some(2));

        let last = listing_page(
            &mut pending,
            2,
            json!({"parentId": "f", "offset": 2, "entries": [entry_json("c", "c", false)]}),
        );
        let Ok(BrowserResult::Entries { next_offset, .. }) = last.result else {
            panic!("expected entries: {last:?}");
        };
        assert_eq!(
            next_offset, None,
            "no nextOffset means the folder is complete"
        );
    }

    /// A phone that ignores paging would answer an offset request with its
    /// first page again, and the UI would append a duplicate of it.
    #[test]
    fn a_page_the_phone_did_not_confirm_is_refused() {
        let mut pending = PendingBrowserRequests::default();
        let response = listing_page(
            &mut pending,
            170,
            json!({"parentId": "f", "entries": [entry_json("a", "a", false)]}),
        );
        assert!(response.result.is_err(), "{response:?}");

        let wrong = listing_page(
            &mut pending,
            170,
            json!({"parentId": "f", "offset": 0, "entries": [entry_json("a", "a", false)]}),
        );
        assert!(wrong.result.is_err(), "{wrong:?}");
    }

    #[test]
    fn a_next_offset_that_does_not_advance_is_refused() {
        let mut pending = PendingBrowserRequests::default();
        for bad in [json!(0), json!(-1), json!("2"), json!(u64::MAX)] {
            let response = listing_page(
                &mut pending,
                0,
                json!({"parentId": "f", "offset": 0, "nextOffset": bad,
                       "entries": [entry_json("a", "a", false)]}),
            );
            assert!(response.result.is_err(), "{bad}: {response:?}");
        }
    }

    #[test]
    fn oversized_listings_and_wrong_versions_are_refused() {
        let mut pending = PendingBrowserRequests::default();
        let (id, _) = sent(&mut pending, BrowserRequest::Roots);
        let entries: Vec<Value> = (0..=MAX_ENTRIES)
            .map(|i| entry_json(&format!("e{i}"), "f", false))
            .collect();
        let response = pending
            .resolve(&reply(
                &id,
                json!({"ok": true, "result": {"entries": entries}}),
            ))
            .unwrap();
        assert!(response.result.is_err());

        let (id, _) = sent(&mut pending, BrowserRequest::Roots);
        let wrong = serde_json::to_vec(&json!({
            "browserVersion": 2, "requestId": id, "ok": true, "result": {"entries": []}
        }))
        .unwrap();
        assert!(pending.resolve(&wrong).is_err());
    }

    #[test]
    fn unanswered_requests_expire_and_the_table_is_bounded() {
        let mut pending = PendingBrowserRequests::default();
        let start = now();
        let mut first = None;
        for _ in 0..MAX_PENDING_REQUESTS {
            let (id, _) = pending.begin(&BrowserRequest::Roots, start).unwrap();
            first.get_or_insert(id);
        }
        assert!(pending.begin(&BrowserRequest::Roots, start).is_err());

        let later = start + REQUEST_TIMEOUT;
        let expired = pending.expire(later);
        assert_eq!(expired.len(), MAX_PENDING_REQUESTS);
        assert!(expired.contains(&first.unwrap()));
        assert!(pending.begin(&BrowserRequest::Roots, later).is_ok());
    }

    #[test]
    fn a_reply_reaches_the_ui_exactly_once() {
        let device = "browser-test-reply";
        let (id, _) = begin_request(device, &BrowserRequest::Roots).unwrap();
        let body = reply(&id, json!({"ok": true, "result": {"entries": []}}));

        accept_reply(device, &body).unwrap();
        assert!(
            accept_reply(device, &body).is_err(),
            "a repeat answers nothing"
        );

        let replies = take_replies(device);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].request_id, id);
        assert!(take_replies(device).is_empty());
    }

    #[test]
    fn a_reply_for_one_device_is_not_accepted_for_another() {
        let (id, _) = begin_request("browser-test-a", &BrowserRequest::Roots).unwrap();
        let body = reply(&id, json!({"ok": true, "result": {"entries": []}}));

        assert!(accept_reply("browser-test-b", &body).is_err());
        assert!(accept_reply("browser-test-a", &body).is_ok());
    }

    #[test]
    fn a_failed_send_or_lost_session_is_reported_immediately() {
        let device = "browser-test-fail";
        let (sent_id, _) = begin_request(device, &BrowserRequest::Roots).unwrap();
        fail_request(
            device,
            &sent_id,
            "DeskLink WebRTC feature transport is not ready",
        );
        let (open_id, _) = begin_request(device, &BrowserRequest::Roots).unwrap();
        reset(device, "The phone connection was replaced");

        let replies = take_replies(device);
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].request_id, sent_id);
        assert_eq!(replies[1].request_id, open_id);
        assert!(replies.iter().all(|response| response.result.is_err()));

        // A reply that straggles in from the old session answers nothing.
        let late = reply(&open_id, json!({"ok": true, "result": {"entries": []}}));
        assert!(accept_reply(device, &late).is_err());
    }

    #[test]
    fn clearing_forgets_every_open_request() {
        let mut pending = PendingBrowserRequests::default();
        let (id, _) = sent(&mut pending, BrowserRequest::Roots);
        assert_eq!(pending.clear(), vec![id.clone()]);
        let stale = reply(&id, json!({"ok": true, "result": {"entries": []}}));
        assert!(pending.resolve(&stale).is_err());
    }
}
