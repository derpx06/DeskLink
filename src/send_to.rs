//! "Send to <phone>" from Files: `desklink --send-files [--to <device>]
//! <paths…>`, a Nautilus script per paired phone and a Dolphin service menu.
//!
//! The command runs before GTK starts: it asks the running DeskLink (over
//! its own D-Bus name, the `app.send-files` action) to send the files,
//! starting it first if it is not running. The scripts and the service menu
//! are written by the app itself, under the user's data dir: nothing to
//! install. A script is removed when its phone is unpaired. DeskLink is never
//! the default application for any file type: its "Open With" entries list
//! only types that something else already opens, and the hijacking entries
//! older builds wrote are removed (`refresh_open_with`).

use std::path::{Path, PathBuf};

use gtk::gio;
use gtk::glib::{self, Variant};
use gtk::prelude::*;

use crate::application::DesklinkApplication;

const APP_ID: &str = "derx06.desklink.com";
const APP_PATH: &str = "/derx06/desklink/com";
/// Marks a script DeskLink wrote, with the phone it is for.
const SCRIPT_MARK: &str = "# desklink-device: ";
/// The two helper entries in `applications/`: "Send to Phone" and "Add to
/// DeskLink Bucket" in a file's "Open With" list. Older builds wrote them with
/// `MimeType=application/octet-stream;...`, which made DeskLink the default
/// application for generic binary files.
const OPEN_WITH_FILES: [&str; 2] = [
    "derx06.desklink.com.SendTo.desktop",
    "derx06.desklink.com.Bucket.desktop",
];
/// Set in every entry the current build writes; an entry of ours without it
/// is an old, default-hijacking one.
const OPEN_WITH_MARK: &str = "X-DeskLink-OpenWith=2";
/// File types the entries may be offered for. Never `application/octet-stream`
/// or a wildcard: a type is added only when something else already opens it.
const OPEN_WITH_CANDIDATES: [&str; 15] = [
    "image/jpeg",
    "image/png",
    "image/gif",
    "image/webp",
    "image/heic",
    "video/mp4",
    "video/quicktime",
    "video/webm",
    "audio/mpeg",
    "audio/ogg",
    "audio/flac",
    "audio/x-wav",
    "application/pdf",
    "text/plain",
    "application/zip",
];
/// The Files script that puts the chosen files in the Bucket.
const BUCKET_SCRIPT: &str = "Add to DeskLink Bucket";
const BUCKET_SCRIPT_MARK: &str = "# desklink-bucket";

/// `--send-files` arguments: the phone (if named) and the files.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) device: Option<String>,
    pub(crate) paths: Vec<PathBuf>,
}

/// Parses what follows `--send-files`. Relative paths are taken from `cwd`.
pub(crate) fn parse(args: &[String], cwd: &Path) -> Request {
    let mut device = None;
    let mut paths = Vec::new();
    let mut options = true;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--" if options => options = false,
            "--to" if options => device = rest.next().cloned(),
            path => {
                // Files may hand over URIs (`%U`); local ones only.
                let path = if path.starts_with("file://") {
                    match gio::File::for_uri(path).path() {
                        Some(path) => path,
                        None => continue,
                    }
                } else if path.contains("://") {
                    // A remote location (smb://, sftp://): not a local file.
                    continue;
                } else {
                    PathBuf::from(path)
                };
                paths.push(if path.is_absolute() {
                    path
                } else {
                    cwd.join(path)
                });
            }
        }
    }
    Request { device, paths }
}

/// `desklink --send-files …`: hands the files to the running DeskLink.
pub fn run_cli(args: &[String]) -> glib::ExitCode {
    let cwd = std::env::current_dir().unwrap_or_default();
    let request = parse(args, &cwd);
    let paths: Vec<String> = request
        .paths
        .iter()
        .filter(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    if paths.is_empty() {
        eprintln!("desklink --send-files: no files to send (folders are not sent)");
        return glib::ExitCode::FAILURE;
    }
    let parameter = (request.device.unwrap_or_default(), paths).to_variant();
    activate_running("send-files", parameter, "--send-files")
}

/// `desklink --bucket [--to <phone>] <files…>`: puts the files in that
/// phone's Bucket (the running DeskLink copies them; folders are refused
/// there, out loud).
pub fn run_bucket_cli(args: &[String]) -> glib::ExitCode {
    let cwd = std::env::current_dir().unwrap_or_default();
    let request = parse(args, &cwd);
    let paths: Vec<String> = request
        .paths
        .iter()
        .filter(|path| path.exists())
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    if paths.is_empty() {
        eprintln!("desklink --bucket: nothing to add");
        return glib::ExitCode::FAILURE;
    }
    let parameter = (request.device.unwrap_or_default(), paths).to_variant();
    activate_running("bucket", parameter, "--bucket")
}

/// `desklink --dial <number>`: hands the number to the running DeskLink,
/// which asks before the phone calls.
pub fn run_dial_cli(number: &str) -> glib::ExitCode {
    if crate::device_links::calls::dial_number(number).is_none() {
        eprintln!("desklink --dial: \"{number}\" is not a phone number");
        return glib::ExitCode::FAILURE;
    }
    activate_running("dial", number.to_variant(), "--dial")
}

/// What the Insert buttons ask for: the name on the command line, then the
/// capture kind and where the result goes (the same pairs as the menu's rows,
/// `window::insert::CAPTURES`).
const INSERT_VERBS: [(&str, &str, &str); 5] = [
    ("photo", "photo", "clipboard"),
    ("photo-file", "photo", "file"),
    ("scan", "scan", "file"),
    ("text", "text", "clipboard"),
    ("qr", "qr", "clipboard"),
];

/// The Sound menu's switches, as the command line spells them: the name, the
/// `DaemonCommand::Sound` source and whether it turns on.
const SOUND_VERBS: [(&str, &str, bool); 6] = [
    ("desk-on", "desk-sound", true),
    ("desk-off", "desk-sound", false),
    ("phone-mic-on", "phone-mic", true),
    ("phone-mic-off", "phone-mic", false),
    ("phone-sound-on", "phone-sound", true),
    ("phone-sound-off", "phone-sound", false),
];

/// `[verb] [--to <device id>]` in either order; exactly one verb.
fn parse_verb_and_device(args: &[String]) -> Result<(String, Option<String>), String> {
    let (mut verb, mut device) = (None, None);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--to" => {
                device = Some(rest.next().ok_or("--to needs a device id")?.clone());
            }
            other if other.starts_with('-') => return Err(format!("unknown option {other}")),
            other => {
                if verb.replace(other.to_string()).is_some() {
                    return Err("only one verb at a time".to_string());
                }
            }
        }
    }
    Ok((verb.ok_or("a verb is needed")?, device))
}

/// `--insert` arguments: `(device, kind, target)`.
pub(crate) fn parse_insert(args: &[String]) -> Result<(String, String, String), String> {
    let (verb, device) = parse_verb_and_device(args)?;
    let (_, kind, target) = INSERT_VERBS
        .iter()
        .find(|(name, _, _)| *name == verb)
        .ok_or_else(|| {
            let names: Vec<&str> = INSERT_VERBS.iter().map(|(name, _, _)| *name).collect();
            format!("\"{verb}\" is not one of: {}", names.join(", "))
        })?;
    Ok((
        device.unwrap_or_default(),
        (*kind).to_string(),
        (*target).to_string(),
    ))
}

/// `--sound` arguments: `(device, source, on)`.
pub(crate) fn parse_sound(args: &[String]) -> Result<(String, String, bool), String> {
    let (verb, device) = parse_verb_and_device(args)?;
    let (_, what, on) = SOUND_VERBS
        .iter()
        .find(|(name, _, _)| *name == verb)
        .ok_or_else(|| {
            let names: Vec<&str> = SOUND_VERBS.iter().map(|(name, _, _)| *name).collect();
            format!("\"{verb}\" is not one of: {}", names.join(", "))
        })?;
    Ok((device.unwrap_or_default(), (*what).to_string(), *on))
}

/// `desklink --insert <photo|photo-file|scan|text|qr> [--to <device id>]`.
pub fn run_insert_cli(args: &[String]) -> glib::ExitCode {
    match parse_insert(args) {
        Ok((device, kind, target)) => {
            activate_running("insert", (device, kind, target).to_variant(), "--insert")
        }
        Err(error) => {
            eprintln!("desklink --insert: {error}");
            glib::ExitCode::from(2)
        }
    }
}

/// `desklink --sound <desk-on|desk-off|phone-mic-on|phone-mic-off|
/// phone-sound-on|phone-sound-off> [--to <device id>]`.
pub fn run_sound_cli(args: &[String]) -> glib::ExitCode {
    match parse_sound(args) {
        Ok((device, what, on)) => {
            activate_running("sound", (device, what, on).to_variant(), "--sound")
        }
        Err(error) => {
            eprintln!("desklink --sound: {error}");
            glib::ExitCode::from(2)
        }
    }
}

/// Activates `app.<action>` on the running DeskLink, starting it first when
/// it is not running.
fn activate_running(action: &str, parameter: Variant, flag: &str) -> glib::ExitCode {
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        eprintln!("desklink {flag}: no session bus");
        return glib::ExitCode::FAILURE;
    };
    let call = || {
        bus.call_sync(
            Some(APP_ID),
            APP_PATH,
            "org.gtk.Actions",
            "Activate",
            Some(
                &(
                    action,
                    vec![parameter.clone()],
                    std::collections::HashMap::<String, Variant>::new(),
                )
                    .to_variant(),
            ),
            None,
            gio::DBusCallFlags::NONE,
            5_000,
            gio::Cancellable::NONE,
        )
    };
    if call().is_ok() {
        return glib::ExitCode::SUCCESS;
    }
    // Not running and not D-Bus activatable here (a plain binary install):
    // start it, and try again while it comes up.
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(250));
        if call().is_ok() {
            return glib::ExitCode::SUCCESS;
        }
    }
    eprintln!("desklink {flag}: DeskLink did not start");
    glib::ExitCode::FAILURE
}

/// The `app.send-files` action: `(device, paths)`; `device` "" means the
/// connected phone (the first, if several).
pub(crate) fn add_action(app: &DesklinkApplication) {
    let action = gio::SimpleAction::new("send-files", Some(glib::VariantTy::new("(sas)").unwrap()));
    let weak = app.downgrade();
    action.connect_activate(move |_, parameter| {
        let (Some(app), Some((device, paths))) = (
            weak.upgrade(),
            parameter.and_then(|value| value.get::<(String, Vec<String>)>()),
        ) else {
            return;
        };
        send(&app, &device, &paths);
    });
    app.add_action(&action);
}

/// What a command line request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Direct {
    Insert { kind: String, target: String },
    Sound { what: String, on: bool },
}

impl Direct {
    /// The verb the rate limit counts.
    fn key(&self) -> String {
        match self {
            Direct::Insert { kind, target } => format!("insert:{kind}:{target}"),
            Direct::Sound { what, on } => format!("sound:{what}:{on}"),
        }
    }

    /// The packet type the phone must take for this to work, if any (the
    /// Insert button's own gate).
    fn needs(&self) -> Option<&'static str> {
        match self {
            Direct::Insert { .. } => {
                Some(crate::device_links::packet::PACKET_TYPE_MOUSEPAD_REQUEST)
            }
            Direct::Sound { .. } => None,
        }
    }

    /// The notification title: never silent.
    pub(crate) fn asking(&self, phone: &str) -> String {
        match self {
            Direct::Insert { kind, target } => {
                let what = match (kind.as_str(), target.as_str()) {
                    ("photo", "file") => "a photo as a file",
                    ("photo", _) => "a photo",
                    ("scan", _) => "a document scan",
                    ("text", _) => "text from a picture",
                    ("qr", _) => "a QR code",
                    _ => "something",
                };
                format!("Asking {phone} for {what}")
            }
            Direct::Sound { what, on } => {
                let thing = match what.as_str() {
                    "desk-sound" => "this computer's sound to play there",
                    "phone-mic" => "its microphone",
                    _ => "its sound",
                };
                if *on {
                    format!("Asking {phone} for {thing}")
                } else {
                    format!("Telling {phone} to stop {thing}")
                }
            }
        }
    }
}

/// At most one request per verb in 5 s and ten in a minute: a script cannot
/// flood the phone with questions. Extra ones are ignored (and said).
#[derive(Default)]
pub(crate) struct Limiter {
    last: std::collections::HashMap<String, std::time::Instant>,
    recent: std::collections::VecDeque<std::time::Instant>,
}

impl Limiter {
    pub(crate) const PER_VERB: std::time::Duration = std::time::Duration::from_secs(5);
    pub(crate) const PER_MINUTE: usize = 10;

    pub(crate) fn allow(&mut self, key: &str, now: std::time::Instant) -> bool {
        let minute = std::time::Duration::from_secs(60);
        while self
            .recent
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= minute)
        {
            self.recent.pop_front();
        }
        if self.recent.len() >= Self::PER_MINUTE
            || self
                .last
                .get(key)
                .is_some_and(|at| now.saturating_duration_since(*at) < Self::PER_VERB)
        {
            return false;
        }
        self.last.insert(key.to_string(), now);
        self.recent.push_back(now);
        true
    }
}

/// Which phone a request goes to.
#[derive(Debug)]
pub(crate) enum Pick<'a> {
    Phone(&'a crate::device_links::device::DeviceView),
    /// Several are connected and none was named.
    Many,
    /// None that is connected (yet) and able.
    None,
}

pub(crate) fn pick<'a>(
    devices: &'a [crate::device_links::device::DeviceView],
    named: &str,
    needs: Option<&str>,
) -> Pick<'a> {
    let mut able = devices.iter().filter(|phone| {
        phone.trusted
            && phone.feature_transport_ready
            && (named.is_empty() || phone.id == named)
            && needs.is_none_or(|packet| crate::window::can_send_feature(phone, packet))
    });
    match (able.next(), able.next()) {
        (Some(phone), None) => Pick::Phone(phone),
        (Some(_), Some(_)) => Pick::Many,
        _ => Pick::None,
    }
}

/// What to do after a look for the phone.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Send,
    Wait,
    GiveUp,
    TooMany,
}

/// A DeskLink that has only just started has no phone ready yet: it waits
/// (this many looks, a quarter second apart: five seconds) before saying so.
const LOOKS: u32 = 20;

pub(crate) fn next_step(found: &Pick, looks_left: u32) -> Step {
    match found {
        Pick::Phone(_) => Step::Send,
        Pick::Many => Step::TooMany,
        Pick::None if looks_left > 0 => Step::Wait,
        Pick::None => Step::GiveUp,
    }
}

thread_local! {
    static LIMITER: std::cell::RefCell<Limiter> = std::cell::RefCell::default();
}

fn say(app: &DesklinkApplication, title: &str, body: &str) {
    let notification = gio::Notification::new(title);
    notification.set_body(Some(body));
    notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
    app.send_notification(Some("direct-action"), &notification);
}

/// One request: rate limited, then sent to the phone as soon as one is ready
/// (a cold start gets five seconds), and always said on the desk.
fn run_direct(app: &DesklinkApplication, device: String, request: Direct) {
    let allowed = LIMITER.with(|limiter| {
        limiter
            .borrow_mut()
            .allow(&request.key(), std::time::Instant::now())
    });
    if !allowed {
        say(
            app,
            "Too many requests",
            "DeskLink ignored a request from the command line: wait a few seconds.",
        );
        return;
    }
    look_for_phone(app.downgrade(), device, request, LOOKS);
}

fn look_for_phone(
    weak: glib::WeakRef<DesklinkApplication>,
    device: String,
    request: Direct,
    looks_left: u32,
) {
    use crate::device_links::daemon::DaemonCommand;
    let Some(app) = weak.upgrade() else {
        return;
    };
    let devices = app
        .daemon_handle()
        .map(|daemon| daemon.devices())
        .unwrap_or_default();
    let found = pick(&devices, &device, request.needs());
    match next_step(&found, looks_left) {
        Step::Send => {
            let (Pick::Phone(phone), Some(daemon)) = (found, app.daemon_handle()) else {
                return;
            };
            if matches!(&request, Direct::Insert { kind, .. } if kind == "text")
                && !crate::device_links::webrtc::capture::text_available(&phone.id)
            {
                say(
                    &app,
                    "Text recognition is not available",
                    &format!("{} cannot read text from a picture.", phone.name),
                );
                return;
            }
            say(
                &app,
                &request.asking(&phone.name),
                "On the phone, answer the question there.",
            );
            daemon.send(match request {
                Direct::Insert { kind, target } => {
                    DaemonCommand::Capture(phone.id.clone(), kind, target)
                }
                Direct::Sound { what, on } => DaemonCommand::Sound(phone.id.clone(), what, on),
            });
        }
        Step::Wait => {
            glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
                look_for_phone(weak, device, request, looks_left - 1)
            });
        }
        Step::TooMany => say(
            &app,
            "More than one phone is connected",
            "Say which with --to <device id>.",
        ),
        Step::GiveUp => say(
            &app,
            "No phone is connected",
            "Connect your phone to DeskLink first (or it does not support this).",
        ),
    }
}

/// The `app.insert` and `app.sound` actions: what the Insert and Sound
/// buttons do, for the phone named (or the only connected one). They send the
/// same commands, so the same gates and the same consent on the phone apply;
/// what they skip is a person at the desk's window, so they are rate limited
/// and never silent.
pub(crate) fn add_direct_actions(app: &DesklinkApplication) {
    let insert = gio::SimpleAction::new("insert", Some(glib::VariantTy::new("(sss)").unwrap()));
    let weak = app.downgrade();
    insert.connect_activate(move |_, parameter| {
        let (Some(app), Some((device, kind, target))) = (
            weak.upgrade(),
            parameter.and_then(|value| value.get::<(String, String, String)>()),
        ) else {
            return;
        };
        // Only what the menu offers.
        if !INSERT_VERBS
            .iter()
            .any(|(_, k, t)| *k == kind && *t == target)
        {
            return;
        }
        run_direct(&app, device, Direct::Insert { kind, target });
    });
    app.add_action(&insert);
    let sound = gio::SimpleAction::new("sound", Some(glib::VariantTy::new("(ssb)").unwrap()));
    let weak = app.downgrade();
    sound.connect_activate(move |_, parameter| {
        let (Some(app), Some((device, what, on))) = (
            weak.upgrade(),
            parameter.and_then(|value| value.get::<(String, String, bool)>()),
        ) else {
            return;
        };
        if !SOUND_VERBS.iter().any(|(_, w, _)| *w == what) {
            return;
        }
        run_direct(&app, device, Direct::Sound { what, on });
    });
    app.add_action(&sound);
}

/// The `app.bucket` action: `(device, paths)` into that phone's Bucket
/// ("" the first phone with one).
pub(crate) fn add_bucket_action(app: &DesklinkApplication) {
    let action = gio::SimpleAction::new("bucket", Some(glib::VariantTy::new("(sas)").unwrap()));
    let weak = app.downgrade();
    action.connect_activate(move |_, parameter| {
        let (Some(app), Some((device, paths))) = (
            weak.upgrade(),
            parameter.and_then(|value| value.get::<(String, Vec<String>)>()),
        ) else {
            return;
        };
        to_bucket(
            &app,
            &device,
            paths.into_iter().map(PathBuf::from).collect(),
        );
    });
    app.add_action(&action);
}

fn to_bucket(app: &DesklinkApplication, device: &str, paths: Vec<PathBuf>) {
    let buckets: Vec<String> = crate::device_links::folders::pairs()
        .into_iter()
        .filter(|view| view.pair.bucket)
        .map(|view| view.pair.device)
        .collect();
    let target = if device.is_empty() {
        buckets.first().cloned()
    } else {
        buckets.iter().find(|id| *id == device).cloned()
    };
    let notification = match target {
        None => {
            let notification = gio::Notification::new("The Bucket is off");
            notification.set_body(Some("Turn on the Bucket in DeskLink first."));
            notification
        }
        Some(device) => {
            // Folders go in whole (2 levels); what was left out is told
            // afterwards, when the copy is done.
            let count = paths.len();
            crate::device_links::folders::import(&device, paths, "");
            gio::Notification::new(&match count {
                0 => "Nothing was added to the Bucket".to_string(),
                1 => "Adding to the Bucket".to_string(),
                count => format!("Adding {count} items to the Bucket"),
            })
        }
    };
    notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
    app.send_notification(Some("bucket"), &notification);
}

fn send(app: &DesklinkApplication, device: &str, paths: &[String]) {
    let Some(daemon) = app.daemon_handle() else {
        return;
    };
    let devices = daemon.devices();
    let target = choose_phone(&devices, device);
    let notification = match target {
        Some(phone) => {
            for path in paths {
                daemon.send(crate::device_links::daemon::DaemonCommand::SendFile(
                    phone.id.clone(),
                    PathBuf::from(path),
                ));
            }
            let what = match paths.len() {
                1 => Path::new(&paths[0])
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "1 file".to_string()),
                count => format!("{count} files"),
            };
            let notification = gio::Notification::new(&format!("Sending {what} to {}", phone.name));
            if !phone.feature_transport_ready {
                notification.set_body(Some(&format!(
                    "Waiting for {}: it is sent as soon as the phone is connected.",
                    phone.name
                )));
            }
            notification
        }
        None => {
            let notification = gio::Notification::new("No phone to send to");
            notification.set_body(Some("Pair a phone in DeskLink first."));
            notification
        }
    };
    notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
    app.send_notification(Some("send-files"), &notification);
}

/// The phone to send to: the one named (and only it: a phone that is not
/// paired, or gone, gets nothing), else the connected one, else any paired.
fn choose_phone<'a>(
    devices: &'a [crate::device_links::device::DeviceView],
    named: &str,
) -> Option<&'a crate::device_links::device::DeviceView> {
    let paired = || devices.iter().filter(|candidate| candidate.trusted);
    if !named.is_empty() {
        return paired().find(|candidate| candidate.id == named);
    }
    paired()
        .find(|candidate| candidate.feature_transport_ready)
        .or_else(|| paired().next())
}

/// The command a script or entry runs: this binary, quoted for `sh`.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// `Send to <phone>`, safe as a file name.
pub(crate) fn script_name(phone: &str) -> String {
    let name: String = phone
        .chars()
        .map(|c| if c == '/' || c.is_control() { ' ' } else { c })
        .collect();
    format!("Send to {}", name.trim())
}

/// The Nautilus script for phone `device_id`: Files runs it with the chosen
/// files as arguments, from their folder.
pub(crate) fn script(exe: &Path, device_id: &str) -> String {
    format!(
        "#!/bin/sh\n# Written by DeskLink; removed when this phone is unpaired.\n{SCRIPT_MARK}{device_id}\nexec {} --send-files --to {} -- \"$@\"\n",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(device_id),
    )
}

/// `path` as one quoted `Exec` argument, per the Desktop Entry spec: `"`,
/// `` ` ``, `$` and `\\` are escaped with a backslash inside the quotes, and
/// then every backslash is doubled (string-value escaping applies first), so
/// a literal backslash is four. `%` is a field code everywhere: `%%`.
pub(crate) fn desktop_exec_quoted(path: &Path) -> String {
    let mut quoted = String::from('"');
    for c in path.to_string_lossy().chars() {
        match c {
            '"' | '`' | '$' => {
                quoted.push_str("\\\\");
                quoted.push(c);
            }
            '\\' => quoted.push_str("\\\\\\\\"),
            '%' => quoted.push_str("%%"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// The Files script for the Bucket.
pub(crate) fn bucket_script(exe: &Path) -> String {
    format!(
        "#!/bin/sh\n# Written by DeskLink.\n{BUCKET_SCRIPT_MARK}\nexec {} --bucket -- \"$@\"\n",
        shell_quote(&exe.to_string_lossy()),
    )
}

/// The Dolphin service menu (KDE's "right-click → Send to …").
const SERVICE_MENU: &str = "derx06.desklink.com-send.desktop";

/// Whether GNOME's Files is the file manager here: a GNOME-family desktop,
/// or Nautilus installed.
fn nautilus_wanted(current_desktop: &str, installed: bool) -> bool {
    installed
        || current_desktop.split(':').any(|desktop| {
            ["GNOME", "Unity", "Budgie", "Pantheon"]
                .iter()
                .any(|family| desktop.eq_ignore_ascii_case(family))
        })
}

/// Whether `program` is on `$PATH`.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// Dolphin's service menu: one "Send to <phone>" action per paired phone.
pub(crate) fn dolphin_menu(exe: &Path, phones: &[(String, String)]) -> String {
    let exe = desktop_exec_quoted(exe);
    let ids: Vec<String> = (0..phones.len())
        .map(|index| format!("phone{index}"))
        .collect();
    let mut menu = format!(
        "[Desktop Entry]\nType=Service\nMimeType=application/octet-stream;\nActions={};\nX-KDE-Priority=TopLevel\nX-KDE-Submenu=Send to Phone\n",
        ids.join(";")
    );
    for ((id, name), action) in phones.iter().zip(&ids) {
        let name: String = name.chars().filter(|c| !c.is_control()).collect();
        menu.push_str(&format!(
            "\n[Desktop Action {action}]\nName=Send to {name}\nIcon=derx06.desklink.com\nExec={exe} --send-files --to {id} -- %F\n"
        ));
    }
    menu
}

/// Whether `text` is a helper entry DeskLink itself wrote (see
/// `OPEN_WITH_FILES`), old or new: the header, our icon and one of our two
/// commands.
fn is_open_with_entry(text: &str) -> bool {
    text.starts_with("[Desktop Entry]\n")
        && text.lines().any(|line| line == "Icon=derx06.desklink.com")
        && text.lines().any(|line| {
            line.starts_with("Exec=")
                && (line.ends_with(" --send-files %F") || line.ends_with(" --bucket %F"))
        })
}

/// `mimeapps.list` text without DeskLink's helper entries: a line that only
/// named them goes, a line that also names other applications keeps those.
/// Everything else is returned byte for byte. `None` when nothing changed.
fn strip_legacy_associations(text: &str) -> Option<String> {
    let mut changed = false;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let comment = line.trim_start().starts_with('#');
        if comment || !OPEN_WITH_FILES.iter().any(|name| line.contains(name)) {
            out.push_str(line);
            continue;
        }
        changed = true;
        let end = &line[line.trim_end_matches(['\n', '\r']).len()..];
        let body = &line[..line.len() - end.len()];
        let Some((key, value)) = body.split_once('=') else {
            continue;
        };
        let kept: Vec<&str> = value
            .split(';')
            .filter(|app| !app.is_empty() && !OPEN_WITH_FILES.contains(app))
            .collect();
        if !kept.is_empty() {
            out.push_str(&format!("{key}={};{end}", kept.join(";")));
        }
    }
    changed.then_some(out)
}

/// Replaces `path` (through a symlink, as dotfile managers make them) with
/// `text`: a temporary file beside it, then a rename.
fn rewrite_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    let target = std::fs::canonicalize(path)?;
    let mut temp = target.clone().into_os_string();
    temp.push(format!(".desklink-{}", std::process::id()));
    let temp = PathBuf::from(temp);
    std::fs::write(&temp, text)?;
    let renamed = std::fs::metadata(&target)
        .and_then(|meta| std::fs::set_permissions(&temp, meta.permissions()))
        .and_then(|()| std::fs::rename(&temp, &target));
    if renamed.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    renamed
}

/// The types of `candidates` that already have a default application other
/// than DeskLink (`default_for_type`: the id of the default, if any).
pub(crate) fn open_with_types(
    candidates: &[&str],
    default_for_type: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    candidates
        .iter()
        .filter(|mime| default_for_type(mime).is_some_and(|id| !id.starts_with(APP_ID)))
        .map(|mime| mime.to_string())
        .collect()
}

/// An "Open With" helper entry offered for `types`.
fn open_with_helper(name: &str, comment: &str, exe: &Path, flag: &str, types: &[String]) -> String {
    let exe = desktop_exec_quoted(exe);
    format!(
        "[Desktop Entry]\nType=Application\nName={name}\nComment={comment}\nIcon=derx06.desklink.com\nExec={exe} {flag} %F\nTerminal=false\nNoDisplay=true\nMimeType={};\n{OPEN_WITH_MARK}\n",
        types.join(";")
    )
}

/// Brings the "Open With" helper entries in step with this system, at every
/// start: our own entries (and, once, the lines naming them in the two
/// `mimeapps.list` files) are removed first, so they cannot answer the
/// question of what the default is; then both entries are written for the
/// candidate types that already have a default that is not DeskLink, and not
/// at all when there are none. `mimeapps.list` is never written otherwise.
/// Whether any file changed.
pub(crate) fn refresh_open_with(
    data_dir: &Path,
    config_dir: &Path,
    exe: &Path,
    default_for_type: impl Fn(&str) -> Option<String>,
) -> bool {
    let applications = data_dir.join("applications");
    let read = || -> Vec<Option<String>> {
        OPEN_WITH_FILES
            .iter()
            .map(|name| {
                std::fs::read_to_string(applications.join(name))
                    .ok()
                    .filter(|text| is_open_with_entry(text))
            })
            .collect()
    };
    let before = read();
    let mut changed = false;
    // An old entry (no mark), or none of ours at all (a user who deleted them
    // may still have the association lines): the lines go. Entries of this
    // build are left to the user's own choices.
    let current = before
        .iter()
        .flatten()
        .all(|text| text.contains(OPEN_WITH_MARK));
    if !current || before.iter().all(Option::is_none) {
        for list in [
            config_dir.join("mimeapps.list"),
            applications.join("mimeapps.list"),
        ] {
            if let Some(clean) = std::fs::read_to_string(&list)
                .ok()
                .and_then(|text| strip_legacy_associations(&text))
            {
                changed |= rewrite_atomically(&list, &clean).is_ok();
            }
        }
    }
    for (name, text) in OPEN_WITH_FILES.iter().zip(&before) {
        if text.is_some() {
            let _ = std::fs::remove_file(applications.join(name));
        }
    }
    let types = open_with_types(&OPEN_WITH_CANDIDATES, default_for_type);
    if !types.is_empty() {
        let entries = [
            open_with_helper(
                "Send to Phone",
                "Send files to your phone with DeskLink",
                exe,
                "--send-files",
                &types,
            ),
            open_with_helper(
                "Add to DeskLink Bucket",
                "Put files in the Bucket you share with your phone",
                exe,
                "--bucket",
                &types,
            ),
        ];
        for (name, text) in OPEN_WITH_FILES.iter().zip(&entries) {
            if let Err(error) = write_if_changed(&applications.join(name), text, false) {
                eprintln!("[DeskLink] Could not add {name} to Open With: {error}");
            }
        }
    }
    changed || read() != before
}

/// Rebuilds the desktop's MIME cache for `<data>/applications`; best effort.
fn update_desktop_database(applications: &Path) {
    let _ = std::process::Command::new("update-desktop-database")
        .arg(applications)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Writes `content` to `path` when it differs; `executable` sets mode 0755.
fn write_if_changed(path: &Path, content: &str, executable: bool) -> std::io::Result<()> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, content)?;
    if executable {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// One script per paired phone (`(id, name)`), and none for any other.
fn sync_scripts(dir: &Path, exe: &Path, phones: &[(String, String)]) -> std::io::Result<()> {
    // Ours, by the phone each is for.
    let mut existing: Vec<(PathBuf, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Some(id) = text.lines().find_map(|line| line.strip_prefix(SCRIPT_MARK)) {
                    existing.push((path, id.to_string()));
                }
            }
        }
    }
    for (path, id) in &existing {
        if !phones.iter().any(|(phone, _)| phone == id) {
            std::fs::remove_file(path)?;
        }
    }
    for (id, name) in phones {
        // Two phones with one name: tell them apart.
        let duplicate = phones.iter().filter(|(_, other)| other == name).count() > 1;
        let mut file = script_name(name);
        if duplicate {
            file.push_str(&format!(" ({})", id.chars().take(6).collect::<String>()));
        }
        // A renamed phone: its old script goes.
        for (path, owner) in &existing {
            if owner == id && path.file_name() != Some(std::ffi::OsStr::new(&file)) {
                let _ = std::fs::remove_file(path);
            }
        }
        write_if_changed(&dir.join(file), &script(exe, id), true)?;
    }
    Ok(())
}

/// Whether the file-manager integration (Nautilus scripts, the Dolphin menu)
/// is written: not from a sandbox the host cannot see into.
fn writes_host_integration(in_flatpak: bool) -> bool {
    !in_flatpak
}

/// Keeps the scripts in step with the paired phones while the app runs.
pub fn start(app: &DesklinkApplication) {
    // Inside a Flatpak these files would land in the app's private data
    // directory, which the host's file managers never read, with an `Exec=`
    // that only exists in the sandbox: nothing is written.
    if !writes_host_integration(crate::device_links::pty::host::in_flatpak()) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let data = glib::user_data_dir();
    // Off the UI thread: file IO, the MIME database and a child process.
    let (open_data, open_config, open_exe) = (data.clone(), glib::user_config_dir(), exe.clone());
    std::thread::spawn(move || {
        let default_for_type = |mime: &str| {
            gio::AppInfo::default_for_type(mime, false)
                .and_then(|app| app.id())
                .map(|id| id.to_string())
        };
        if refresh_open_with(&open_data, &open_config, &open_exe, default_for_type) {
            update_desktop_database(&open_data.join("applications"));
        }
    });
    let scripts = data.join("nautilus").join("scripts");
    let service_menu = data.join("kio").join("servicemenus").join(SERVICE_MENU);
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    // Each file manager's extra only where it is used.
    let nautilus = nautilus_wanted(&desktop, on_path("nautilus"));
    let dolphin = crate::kde::is_kde(&desktop) || on_path("dolphin");
    // The paired phones the scripts were last written for.
    let last = std::cell::RefCell::new(None::<Vec<(String, String)>>);
    let weak = app.downgrade();
    crate::ticker::every_tick(move || {
        let Some(app) = weak.upgrade().filter(|app| !app.is_remote()) else {
            return glib::ControlFlow::Break;
        };
        let Some(daemon) = app.daemon_handle() else {
            return glib::ControlFlow::Continue;
        };
        let mut phones: Vec<(String, String)> = daemon
            .devices()
            .into_iter()
            .filter(|device| device.trusted)
            .map(|device| (device.id, device.name))
            .collect();
        phones.sort();
        if last.borrow().as_ref() != Some(&phones) {
            if nautilus {
                if let Err(error) = sync_scripts(&scripts, &exe, &phones) {
                    eprintln!("[DeskLink] Could not update the Files scripts: {error}");
                }
                // F8b: "Add to DeskLink Bucket", while a phone is paired.
                let bucket = scripts.join(BUCKET_SCRIPT);
                let _ = if phones.is_empty() {
                    std::fs::remove_file(&bucket)
                } else {
                    write_if_changed(&bucket, &bucket_script(&exe), true)
                };
            }
            if dolphin {
                let written = if phones.is_empty() {
                    match std::fs::remove_file(&service_menu) {
                        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
                        _ => Ok(()),
                    }
                } else {
                    write_if_changed(&service_menu, &dolphin_menu(&exe, &phones), true)
                };
                if let Err(error) = written {
                    eprintln!("[DeskLink] Could not update the Dolphin menu: {error}");
                }
            }
            last.replace(Some(phones));
        }
        glib::ControlFlow::Continue
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    /// `--insert`: the verb in any place relative to `--to`, only the verbs
    /// the Insert menu has, and each one asks for what its row asks for.
    fn phone(id: &str, ready: bool) -> crate::device_links::device::DeviceView {
        let info = crate::device_links::device_info::DeviceInfo::local(
            id.to_string(),
            format!("Name {id}"),
        );
        let mut device =
            crate::device_links::device::DeviceView::from_info(&info, "10.0.0.2".into(), true);
        device.trusted = true;
        device.feature_transport_ready = ready;
        device.incoming_capabilities =
            vec![crate::device_links::packet::PACKET_TYPE_MOUSEPAD_REQUEST.to_string()];
        device
    }

    /// One a verb every five seconds, ten in a minute, and the rest ignored
    /// (fake clock).
    #[test]
    fn requests_from_a_script_are_rate_limited() {
        use std::time::{Duration, Instant};
        let (mut limiter, t0) = (Limiter::default(), Instant::now());
        let at = |s: u64| t0 + Duration::from_secs(s);
        assert!(limiter.allow("insert:photo:clipboard", at(0)));
        assert!(
            !limiter.allow("insert:photo:clipboard", at(4)),
            "same verb, too soon"
        );
        assert!(limiter.allow("sound:phone-mic:true", at(4)), "another verb");
        assert!(limiter.allow("insert:photo:clipboard", at(5)));
        // Ten in the minute: the eleventh is ignored, whichever verb.
        let mut limiter = Limiter::default();
        for n in 0..10u64 {
            assert!(limiter.allow(&format!("v{n}"), at(n)), "{n}");
        }
        assert!(
            !limiter.allow("fresh", at(30)),
            "ten already in this minute"
        );
        assert!(
            limiter.allow("fresh", at(60)),
            "the first has left the minute"
        );
    }

    /// Never silent: every request says what is being asked, of whom.
    #[test]
    fn every_request_is_worded_on_the_desk() {
        let ask = |d: Direct| d.asking("Pixel");
        assert_eq!(
            ask(Direct::Insert {
                kind: "photo".into(),
                target: "clipboard".into()
            }),
            "Asking Pixel for a photo"
        );
        assert_eq!(
            ask(Direct::Insert {
                kind: "scan".into(),
                target: "file".into()
            }),
            "Asking Pixel for a document scan"
        );
        assert_eq!(
            ask(Direct::Sound {
                what: "phone-mic".into(),
                on: true
            }),
            "Asking Pixel for its microphone"
        );
        assert_eq!(
            ask(Direct::Sound {
                what: "desk-sound".into(),
                on: true
            }),
            "Asking Pixel for this computer's sound to play there"
        );
        assert_eq!(
            ask(Direct::Sound {
                what: "phone-sound".into(),
                on: false
            }),
            "Telling Pixel to stop its sound"
        );
    }

    /// The phone: the one named, else the only connected one; two connected
    /// and none named is refused; an Insert needs the Insert button's gate.
    #[test]
    fn a_request_goes_to_one_ready_phone() {
        let mousepad = Some(crate::device_links::packet::PACKET_TYPE_MOUSEPAD_REQUEST);
        let one = [phone("a", true), phone("b", false)];
        assert!(matches!(pick(&one, "", mousepad), Pick::Phone(p) if p.id == "a"));
        assert!(matches!(pick(&one, "a", None), Pick::Phone(_)));
        assert!(
            matches!(pick(&one, "b", None), Pick::None),
            "named but not connected"
        );
        let two = [phone("a", true), phone("b", true)];
        assert!(matches!(pick(&two, "", None), Pick::Many));
        assert!(matches!(pick(&two, "b", None), Pick::Phone(p) if p.id == "b"));
        let mut old = phone("c", true);
        old.incoming_capabilities.clear();
        assert!(
            matches!(pick(&[old.clone()], "", mousepad), Pick::None),
            "cannot do Insert"
        );
        assert!(
            matches!(pick(&[old], "", None), Pick::Phone(_)),
            "Sound has no such gate"
        );
    }

    /// A cold start: no phone is ready yet, so it waits (five seconds of
    /// looks) before saying there is none; one that shows up is used.
    #[test]
    fn a_cold_start_waits_for_a_phone_before_giving_up() {
        assert_eq!(next_step(&Pick::None, LOOKS), Step::Wait);
        assert_eq!(next_step(&Pick::None, 1), Step::Wait);
        assert_eq!(next_step(&Pick::None, 0), Step::GiveUp);
        let ready = phone("a", true);
        assert_eq!(next_step(&Pick::Phone(&ready), 0), Step::Send);
        assert_eq!(next_step(&Pick::Many, LOOKS), Step::TooMany);
        assert_eq!(LOOKS as u64 * 250, 5000);
    }

    #[test]
    fn insert_arguments_name_a_menu_row_and_optionally_a_phone() {
        assert_eq!(
            parse_insert(&strings(&["photo"])),
            Ok((String::new(), "photo".into(), "clipboard".into()))
        );
        assert_eq!(
            parse_insert(&strings(&["--to", "p1", "scan"])),
            Ok(("p1".into(), "scan".into(), "file".into()))
        );
        assert_eq!(
            parse_insert(&strings(&["qr", "--to", "p2"])),
            Ok(("p2".into(), "qr".into(), "clipboard".into()))
        );
        assert_eq!(parse_insert(&strings(&["photo-file"])).unwrap().2, "file");
        for bad in [
            strings(&[]),
            strings(&["video"]),
            strings(&["photo", "scan"]),
            strings(&["photo", "--to"]),
            strings(&["--wat", "photo"]),
        ] {
            assert!(parse_insert(&bad).is_err(), "{bad:?}");
        }
        // Every verb is a row of the menu: nothing the buttons cannot do.
        for (name, kind, target) in INSERT_VERBS {
            assert!(
                crate::window::insert::CAPTURES
                    .iter()
                    .any(|(k, t, _, _)| *k == kind && *t == target),
                "{name}"
            );
        }
    }

    /// `--sound`: the six switches of the Sound menu, on and off.
    #[test]
    fn sound_arguments_name_a_switch_and_optionally_a_phone() {
        assert_eq!(
            parse_sound(&strings(&["desk-on"])),
            Ok((String::new(), "desk-sound".into(), true))
        );
        assert_eq!(
            parse_sound(&strings(&["phone-mic-off", "--to", "p1"])),
            Ok(("p1".into(), "phone-mic".into(), false))
        );
        assert_eq!(
            parse_sound(&strings(&["phone-sound-on"])),
            Ok((String::new(), "phone-sound".into(), true))
        );
        for bad in [
            strings(&[]),
            strings(&["on"]),
            strings(&["desk-sound-on"]),
            strings(&["desk-on", "phone-mic-on"]),
            strings(&["-x"]),
        ] {
            assert!(parse_sound(&bad).is_err(), "{bad:?}");
        }
        let names: Vec<_> = SOUND_VERBS.iter().map(|(name, _, _)| *name).collect();
        assert_eq!(names.len(), 6);
        for (_, what, _) in SOUND_VERBS {
            assert!(["desk-sound", "phone-mic", "phone-sound"].contains(&what));
        }
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn the_command_line_takes_a_phone_and_files() {
        let cwd = Path::new("/home/me/Pictures");
        let request = parse(
            &args(&["--to", "p1", "--", "a.jpg", "/tmp/b.pdf", "--to"]),
            cwd,
        );
        assert_eq!(request.device.as_deref(), Some("p1"));
        assert_eq!(
            request.paths,
            [
                PathBuf::from("/home/me/Pictures/a.jpg"),
                PathBuf::from("/tmp/b.pdf"),
                PathBuf::from("/home/me/Pictures/--to"),
            ],
            "after --, everything is a file"
        );
        let uris = parse(&args(&["file:///tmp/x%20y.png", "smb://host/z"]), cwd);
        assert_eq!(uris.device, None);
        assert_eq!(
            uris.paths,
            [PathBuf::from("/tmp/x y.png")],
            "remote files skipped"
        );
    }

    #[test]
    fn scripts_quote_everything_and_name_the_phone() {
        let script = script(Path::new("/opt/my apps/desklink"), "it's-p1");
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains("# desklink-device: it's-p1\n"));
        assert!(script
            .contains("exec '/opt/my apps/desklink' --send-files --to 'it'\\''s-p1' -- \"$@\"\n"));
        assert_eq!(script_name("Pixel/9 Pro"), "Send to Pixel 9 Pro");
    }

    #[test]
    fn desktop_exec_quoting() {
        assert_eq!(
            desktop_exec_quoted(Path::new("/a\\b/$c/100%/d`e")),
            "\"/a\\\\\\\\b/\\\\$c/100%%/d\\\\`e\"",
            "a backslash is four, a dollar two plus itself, a percent doubled"
        );
    }

    #[test]
    fn a_named_phone_gets_the_files_or_nobody_does() {
        let phone = |id: &str, trusted: bool, ready: bool| {
            let info = crate::device_links::device_info::DeviceInfo {
                id: id.into(),
                name: id.into(),
                device_type: "phone".into(),
                protocol_version: 9,
                incoming_capabilities: Vec::new(),
                outgoing_capabilities: Vec::new(),
                instance_id: None,
                app_version: None,
            };
            let mut device =
                crate::device_links::device::DeviceView::from_info(&info, String::new(), trusted);
            device.feature_transport_ready = ready;
            device
        };
        let devices = [
            phone("p1", true, true),
            phone("p2", true, false),
            phone("x", false, true),
        ];
        let id =
            |found: Option<&crate::device_links::device::DeviceView>| found.map(|d| d.id.clone());
        assert_eq!(
            id(choose_phone(&devices, "p2")),
            Some("p2".into()),
            "the one named"
        );
        assert_eq!(
            id(choose_phone(&devices, "gone")),
            None,
            "never another phone"
        );
        assert_eq!(id(choose_phone(&devices, "x")), None, "not paired");
        assert_eq!(
            id(choose_phone(&devices, "")),
            Some("p1".into()),
            "unnamed: the connected one"
        );
    }

    #[test]
    fn dolphin_gets_one_action_per_phone() {
        let menu = dolphin_menu(
            Path::new("/usr/bin/desklink"),
            &[("p1".into(), "Pixel".into()), ("p2".into(), "Tab".into())],
        );
        assert!(menu.starts_with("[Desktop Entry]\nType=Service\n"));
        assert!(menu.contains("Actions=phone0;phone1;\n"));
        assert!(menu.contains(
            "[Desktop Action phone1]\nName=Send to Tab\nIcon=derx06.desklink.com\nExec=\"/usr/bin/desklink\" --send-files --to p2 -- %F\n"
        ));
        assert!(nautilus_wanted("ubuntu:GNOME", false));
        assert!(!nautilus_wanted("KDE", false));
        assert!(nautilus_wanted("KDE", true), "installed anyway");
    }

    #[test]
    fn scripts_follow_the_paired_phones() {
        let dir = std::env::temp_dir().join(format!("desklink-scripts-{}", uuid::Uuid::new_v4()));
        let exe = Path::new("/usr/bin/desklink");
        let mine = std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(dir.join("My own script"), "#!/bin/sh\necho hi\n"));
        mine.unwrap();
        let phones = vec![
            ("p1".to_string(), "Pixel".to_string()),
            ("p2".to_string(), "Tab".to_string()),
        ];
        sync_scripts(&dir, exe, &phones).unwrap();
        assert!(dir.join("Send to Pixel").exists() && dir.join("Send to Tab").exists());
        sync_scripts(&dir, exe, &phones[..1]).unwrap();
        assert!(!dir.join("Send to Tab").exists(), "unpaired: removed");
        assert!(
            dir.join("My own script").exists(),
            "the user's own scripts are left alone"
        );
        let renamed = vec![("p1".to_string(), "Pixel 9".to_string())];
        sync_scripts(&dir, exe, &renamed).unwrap();
        assert!(dir.join("Send to Pixel 9").exists() && !dir.join("Send to Pixel").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    const OLD_BUCKET: &str = "[Desktop Entry]\nType=Application\nName=Add to DeskLink Bucket\nComment=Put files in the Bucket you share with your phone\nIcon=derx06.desklink.com\nExec=\"/home/me/.local/bin/desklink\" --bucket %F\nTerminal=false\nNoDisplay=true\nMimeType=application/octet-stream;image/jpeg;\n";
    const OLD_SEND: &str = "[Desktop Entry]\nType=Application\nName=Send to Phone\nComment=Send files to your phone with DeskLink\nIcon=derx06.desklink.com\nExec=/usr/bin/desklink --send-files %F\nTerminal=false\nNoDisplay=true\nMimeType=application/octet-stream;image/jpeg;\n";
    const EXE: &str = "/usr/bin/desklink";

    fn fixture() -> (PathBuf, PathBuf, PathBuf) {
        let home = std::env::temp_dir().join(format!("desklink-openwith-{}", uuid::Uuid::new_v4()));
        let (data, config) = (home.join("share"), home.join("config"));
        std::fs::create_dir_all(data.join("applications")).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        (home, data, config)
    }

    /// A system where only these types have a default application.
    fn defaults(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |mime| {
            pairs
                .iter()
                .find(|(m, _)| *m == mime)
                .map(|(_, id)| id.to_string())
        }
    }

    fn mime_line(text: &str) -> String {
        text.lines()
            .find(|line| line.starts_with("MimeType="))
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn only_types_someone_else_already_opens_are_offered() {
        let lookup = defaults(&[
            ("image/png", "org.gnome.Loupe.desktop"),
            ("application/pdf", "evince.desktop"),
            ("text/plain", "derx06.desklink.com.SendTo.desktop"),
            ("video/mp4", "derx06.desklink.com.desktop"),
            ("application/octet-stream", "gedit.desktop"),
        ]);
        let candidates = [
            "image/png",
            "image/gif",
            "text/plain",
            "video/mp4",
            "application/pdf",
        ];
        assert_eq!(
            open_with_types(&candidates, &lookup),
            ["image/png", "application/pdf"],
            "no default: out; our own id: out"
        );
        assert!(open_with_types(&candidates, |_| None).is_empty());
        assert!(
            !OPEN_WITH_CANDIDATES
                .iter()
                .any(|mime| mime.contains('*') || *mime == "application/octet-stream"),
            "a generic type is never a candidate, whatever its default is"
        );
        assert!(open_with_types(
            &OPEN_WITH_CANDIDATES,
            defaults(&[("application/octet-stream", "gedit.desktop")])
        )
        .is_empty());
    }

    #[test]
    fn the_entries_list_only_those_types_and_say_they_are_current() {
        let (home, data, config) = fixture();
        let lookup = defaults(&[
            ("image/png", "loupe.desktop"),
            ("application/pdf", "evince.desktop"),
        ]);
        assert!(refresh_open_with(&data, &config, Path::new(EXE), &lookup));
        let apps = data.join("applications");
        let send = std::fs::read_to_string(apps.join(OPEN_WITH_FILES[0])).unwrap();
        let bucket = std::fs::read_to_string(apps.join(OPEN_WITH_FILES[1])).unwrap();
        for entry in [&send, &bucket] {
            assert_eq!(mime_line(entry), "MimeType=image/png;application/pdf;");
            assert!(entry.contains(OPEN_WITH_MARK) && entry.contains("NoDisplay=true"));
            assert!(is_open_with_entry(entry));
        }
        assert!(send.contains("Exec=\"/usr/bin/desklink\" --send-files %F\n"));
        assert!(bucket.contains("Exec=\"/usr/bin/desklink\" --bucket %F\n"));
        // Idempotent.
        assert!(!refresh_open_with(&data, &config, Path::new(EXE), &lookup));
        // A type loses its default: it drops out at the next start.
        let fewer = defaults(&[("image/png", "loupe.desktop")]);
        assert!(refresh_open_with(&data, &config, Path::new(EXE), &fewer));
        let send = std::fs::read_to_string(apps.join(OPEN_WITH_FILES[0])).unwrap();
        assert_eq!(mime_line(&send), "MimeType=image/png;");
        // Nothing has a default: no handler file at all.
        assert!(refresh_open_with(&data, &config, Path::new(EXE), |_| None));
        assert!(!apps.join(OPEN_WITH_FILES[0]).exists() && !apps.join(OPEN_WITH_FILES[1]).exists());
        assert!(!refresh_open_with(&data, &config, Path::new(EXE), |_| None));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Our own entries must not decide what the default is: the lookup runs
    /// after they are removed.
    #[test]
    fn the_lookup_does_not_see_our_own_entries() {
        let (home, data, config) = fixture();
        let apps = data.join("applications");
        std::fs::write(apps.join(OPEN_WITH_FILES[0]), OLD_SEND).unwrap();
        let seen = std::cell::Cell::new(false);
        refresh_open_with(&data, &config, Path::new(EXE), |_| {
            seen.set(seen.get() || apps.join(OPEN_WITH_FILES[0]).exists());
            None
        });
        assert!(!seen.get());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The owner's machine: the old files go, the mimeapps lines naming them
    /// go, and nothing else moves; the new entries do not include the type
    /// that had no other default.
    #[test]
    fn the_old_entries_and_their_associations_are_replaced_and_nothing_else_moves() {
        let (home, data, config) = fixture();
        let apps = data.join("applications");
        std::fs::write(apps.join(OPEN_WITH_FILES[1]), OLD_BUCKET).unwrap();
        std::fs::write(apps.join(OPEN_WITH_FILES[0]), OLD_SEND).unwrap();
        let user_entry = "[Desktop Entry]\nType=Application\nName=Mine\nExec=mine %F\n";
        std::fs::write(apps.join("derx06.desklink.com.desktop"), user_entry).unwrap();
        std::fs::write(apps.join("other.desktop"), user_entry).unwrap();
        let before = "# keep me\r\n[Default Applications]\napplication/octet-stream=derx06.desklink.com.Bucket.desktop;\ntext/plain=gedit.desktop;derx06.desklink.com.SendTo.desktop;\n\n[Added Associations]\nimage/png=derx06.desklink.com.SendTo.desktop\nimage/jpeg=eog.desktop\n# derx06.desklink.com.Bucket.desktop=stays\naudio/ogg=vlc.desktop";
        let after = "# keep me\r\n[Default Applications]\ntext/plain=gedit.desktop;\n\n[Added Associations]\nimage/jpeg=eog.desktop\n# derx06.desklink.com.Bucket.desktop=stays\naudio/ogg=vlc.desktop";
        std::fs::write(config.join("mimeapps.list"), before).unwrap();
        std::fs::write(apps.join("mimeapps.list"), before).unwrap();

        let lookup = defaults(&[
            ("image/jpeg", "eog.desktop"),
            ("text/plain", "gedit.desktop"),
        ]);
        assert!(refresh_open_with(&data, &config, Path::new(EXE), &lookup));
        for list in [config.join("mimeapps.list"), apps.join("mimeapps.list")] {
            assert_eq!(std::fs::read_to_string(list).unwrap(), after);
        }
        for name in OPEN_WITH_FILES {
            let text = std::fs::read_to_string(apps.join(name)).unwrap();
            assert_eq!(mime_line(&text), "MimeType=image/jpeg;text/plain;");
            assert!(!text.contains("octet-stream"));
        }
        for kept in ["derx06.desklink.com.desktop", "other.desktop"] {
            assert_eq!(
                std::fs::read_to_string(apps.join(kept)).unwrap(),
                user_entry
            );
        }
        // Idempotent, and now that the entries are this build's the user's
        // own choices in mimeapps.list are not touched.
        let chosen = format!(
            "{after}\n[Default Applications]\nimage/png=derx06.desklink.com.SendTo.desktop\n"
        );
        std::fs::write(config.join("mimeapps.list"), &chosen).unwrap();
        assert!(!refresh_open_with(&data, &config, Path::new(EXE), &lookup));
        assert_eq!(
            std::fs::read_to_string(config.join("mimeapps.list")).unwrap(),
            chosen
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// With no entry of ours on disk the lines naming them go too, so a
    /// re-created file cannot inherit an old default.
    #[test]
    fn stale_associations_go_even_when_the_files_are_gone() {
        let (home, data, config) = fixture();
        std::fs::write(
            config.join("mimeapps.list"),
            "[Default Applications]\napplication/octet-stream=derx06.desklink.com.Bucket.desktop\nx/y=z.desktop\n",
        )
        .unwrap();
        assert!(refresh_open_with(&data, &config, Path::new(EXE), |_| None));
        assert_eq!(
            std::fs::read_to_string(config.join("mimeapps.list")).unwrap(),
            "[Default Applications]\nx/y=z.desktop\n"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A file with our name that is not what we wrote is the user's.
    #[test]
    fn a_modified_file_with_our_name_is_left_alone() {
        let (home, data, config) = fixture();
        let path = data.join("applications").join(OPEN_WITH_FILES[1]);
        let theirs = "[Desktop Entry]\nType=Application\nName=Bucket\nExec=my-script %F\n";
        std::fs::write(&path, theirs).unwrap();
        let _ = refresh_open_with(&data, &config, Path::new(EXE), |_| None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A `mimeapps.list` kept in a dotfiles repo is a symlink: the target is
    /// rewritten, the link stays.
    #[test]
    fn a_mimeapps_symlink_stays_a_symlink() {
        let (home, data, config) = fixture();
        let target = home.join("dotfiles-mimeapps.list");
        std::fs::write(
            &target,
            "[Default Applications]\na/b=derx06.desklink.com.SendTo.desktop;\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(&target, config.join("mimeapps.list")).unwrap();
        assert!(refresh_open_with(&data, &config, Path::new(EXE), |_| None));
        assert!(std::fs::symlink_metadata(config.join("mimeapps.list"))
            .unwrap()
            .is_symlink());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "[Default Applications]\n"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// DeskLink must never be the default application for a file type. The
    /// only `MimeType=` it may carry: its own launcher takes `tel:` links; the
    /// "Open With" helpers list candidate types that already have another
    /// default (never a generic one); the Dolphin service menu (in
    /// `kio/servicemenus`: it has no `Exec`, is no application, is never put
    /// in the MIME cache and is no handler).
    #[test]
    fn nothing_desklink_writes_or_ships_makes_it_the_default_for_a_file_type() {
        fn mime_lines(text: &str) -> Vec<String> {
            text.lines()
                .filter(|line| line.trim_start().starts_with("MimeType"))
                .map(str::to_string)
                .collect()
        }
        let exe = Path::new(EXE);
        let menu = dolphin_menu(exe, &[("p1".into(), "Pixel".into())]);
        assert_eq!(mime_lines(&menu), ["MimeType=application/octet-stream;"]);
        assert!(
            menu.starts_with("[Desktop Entry]\nType=Service\n"),
            "not an Application"
        );
        let main_group = menu.split("\n[Desktop Action").next().unwrap();
        assert!(
            !main_group.contains("\nExec=") && !main_group.contains("\nTryExec="),
            "no handler command of its own"
        );
        for written in [
            crate::autostart::entry(EXE),
            crate::kde::RUNNER.to_string(),
            script(exe, "p1"),
            bucket_script(exe),
        ] {
            assert!(mime_lines(&written).is_empty(), "{written}");
        }
        // The helpers, for every candidate: nothing generic, ever.
        let all: Vec<String> = OPEN_WITH_CANDIDATES.iter().map(|m| m.to_string()).collect();
        let helper = open_with_helper("n", "c", exe, "--bucket", &all);
        let line = mime_line(&helper);
        assert!(!line.contains("octet-stream") && !line.contains('*') && !line.contains("inode/"));
        // Every shipped file: only the launcher's `tel:` scheme.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut found = Vec::new();
        let mut folders = vec![
            root.join("data"),
            root.join("packaging"),
            root.join("flatpak"),
        ];
        while let Some(dir) = folders.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    folders.push(path);
                } else if let Ok(text) = std::fs::read_to_string(&path) {
                    for line in mime_lines(&text) {
                        found.push((
                            path.file_name().unwrap().to_string_lossy().to_string(),
                            line,
                        ));
                    }
                }
            }
        }
        assert_eq!(
            found,
            [(
                "derx06.desklink.com.desktop.in".to_string(),
                "MimeType=x-scheme-handler/tel;".to_string()
            )]
        );
        assert!(is_open_with_entry(OLD_BUCKET) && is_open_with_entry(OLD_SEND));
        assert!(!OLD_SEND.contains(OPEN_WITH_MARK));
    }

    /// A sandbox writes no host file-manager integration.
    #[test]
    fn a_flatpak_writes_no_host_integration() {
        assert!(writes_host_integration(false));
        assert!(!writes_host_integration(true));
    }
}
