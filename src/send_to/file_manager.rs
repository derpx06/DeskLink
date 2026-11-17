//! The file-manager side of "Send to phone": what a request may carry (the
//! refusals, said out loud), and the integration files DeskLink writes under
//! the user's own directories, all of them removed again by one switch.
//!
//! Trust model: a request names paths; the desk READS them as the user, the
//! same as dragging a file onto the window. The `app.send-files` action is on
//! the session bus (one user's bus; a sandboxed app cannot reach it without
//! `--talk-name`), so it is a convenience for the user's own processes, not a
//! boundary against them. It is still validated at action time (regular,
//! readable file; not DeskLink's own state), capped (500 entries) and rate
//! limited, and a send is always announced.
//!
//! Injection safety: nothing a phone or a file name says is ever run by a
//! shell. Phone ids must be plain (`safe_id`, others get no entry); names are
//! cleaned (`clean_name`) and escaped for the file format they land in; the
//! Nautilus extension is fixed code that reads a JSON file and starts the
//! program with an argument list.
//!
//! Ownership: a file is written or removed only when it carries one of our
//! markers (`write_marked`, `remove_if_ours`): a user's own file at one of the
//! fixed names is never overwritten and never deleted. Writes are atomic (a
//! temporary file in the same folder, fsync, rename). One phone's failure
//! (a name too long for a file name) never stops the others.
//!
//! Validation happens when the request arrives (on a worker thread); the
//! file is read later, possibly from the waiting queue (up to 24 h): the same
//! trust domain, so there is no attempt to close that window.
//!
//! Sibling edges: none. Parent (`send_to`): the script and menu writers.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use super::*;

/// Entries one request may carry.
pub(crate) const MAX_ENTRIES: usize = 500;

/// Why entries of a request were not sent.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Skipped {
    pub(crate) folders: usize,
    pub(crate) missing: usize,
    pub(crate) unreadable: usize,
    pub(crate) special: usize,
    pub(crate) protected: usize,
    pub(crate) remote: usize,
    /// File names that are not valid text (cannot cross to the desk intact).
    pub(crate) badname: usize,
}

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

impl Skipped {
    pub(crate) fn is_empty(&self) -> bool {
        *self == Skipped::default()
    }

    /// What to tell the person, `None` when nothing was skipped.
    pub(crate) fn message(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let only_folders = Skipped {
            folders: self.folders,
            ..Skipped::default()
        } == *self;
        if only_folders {
            return Some("Folders aren't sent yet. Select files.".into());
        }
        let mut parts = Vec::new();
        if self.folders > 0 {
            parts.push("Folders aren't sent yet.".to_string());
        }
        if self.remote > 0 {
            parts.push(format!(
                "{} aren't stored on this computer (network, trash or recent locations): copy them to a folder first.",
                count(self.remote, "item", "items")
            ));
        }
        if self.badname > 0 {
            parts.push(format!(
                "{} {} not valid text and can't be sent.",
                count(self.badname, "file name", "file names"),
                if self.badname == 1 { "is" } else { "are" }
            ));
        }
        if self.missing > 0 {
            parts.push(format!(
                "{} no longer exist.",
                count(self.missing, "file", "files")
            ));
        }
        if self.unreadable > 0 {
            parts.push(format!(
                "{} can't be read.",
                count(self.unreadable, "file", "files")
            ));
        }
        if self.special > 0 {
            parts.push(format!(
                "{} aren't ordinary files.",
                count(self.special, "item", "items")
            ));
        }
        if self.protected > 0 {
            parts.push("DeskLink's own files aren't sent.".to_string());
        }
        Some(parts.join(" "))
    }
}

/// The folders holding DeskLink's own keys and state: never sent.
fn protected_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![glib::user_config_dir().join("desklink")];
    dirs.push(
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
            .unwrap_or_default()
            .join("desklink"),
    );
    dirs.push(glib::user_cache_dir().join("desklink"));
    // The symlink-resolved forms count too.
    let real: Vec<PathBuf> = dirs.iter().filter_map(|d| d.canonicalize().ok()).collect();
    dirs.extend(real);
    dirs
}

/// (device, inode) of the files inside the protected folders (a few levels,
/// a few hundred entries): a hard link elsewhere is the same file.
fn protected_ids(dirs: &[PathBuf]) -> std::collections::HashSet<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let mut ids = std::collections::HashSet::new();
    let mut folders: Vec<(PathBuf, u8)> = dirs.iter().map(|d| (d.clone(), 0)).collect();
    while let Some((dir, depth)) = folders.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if ids.len() >= 500 {
                return ids;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                if depth < 3 {
                    folders.push((entry.path(), depth + 1));
                }
            } else if meta.is_file() {
                ids.insert((meta.dev(), meta.ino()));
            }
        }
    }
    ids
}

/// Re-checks every path at action time: an existing, readable, ordinary file
/// (symlinks followed), not under DeskLink's own folders. Returns the
/// sendable paths and why the others are not.
pub(crate) fn classify(paths: &[PathBuf]) -> (Vec<String>, Skipped) {
    let protected = protected_dirs();
    let protected_ids = protected_ids(&protected);
    let mut sendable = Vec::new();
    let mut skipped = Skipped::default();
    for path in paths {
        let Some(text) = path.to_str() else {
            skipped.badname += 1;
            continue;
        };
        if text.contains('\u{FFFD}') {
            skipped.badname += 1;
            continue;
        }
        if !path.is_absolute() {
            skipped.missing += 1;
            continue;
        }
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                skipped.unreadable += 1;
                continue;
            }
            Err(_) => {
                skipped.missing += 1;
                continue;
            }
        };
        if meta.is_dir() {
            skipped.folders += 1;
        } else if !meta.is_file() {
            // A device, FIFO or socket: opening one can block forever.
            skipped.special += 1;
        } else if path
            .canonicalize()
            .is_ok_and(|real| protected.iter().any(|dir| real.starts_with(dir)))
            || protected.iter().any(|dir| path.starts_with(dir))
            || {
                // A hard link to one of our files, from anywhere.
                use std::os::unix::fs::MetadataExt;
                meta.nlink() > 1 && protected_ids.contains(&(meta.dev(), meta.ino()))
            }
        {
            skipped.protected += 1;
        } else if std::fs::File::open(path).is_err() {
            skipped.unreadable += 1;
        } else {
            sendable.push(text.to_string());
        }
    }
    (sendable, skipped)
}

/// At most [`Burst::MAX`] requests in [`Burst::WINDOW`]: a runaway script
/// cannot flood the queue. The rest are ignored (and said).
#[derive(Default)]
pub(crate) struct Burst(std::collections::VecDeque<std::time::Instant>);

impl Burst {
    pub(crate) const MAX: usize = 5;
    pub(crate) const WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

    pub(crate) fn allow(&mut self, now: std::time::Instant) -> bool {
        while self
            .0
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= Self::WINDOW)
        {
            self.0.pop_front();
        }
        if self.0.len() >= Self::MAX {
            return false;
        }
        self.0.push_back(now);
        true
    }
}

/// Files already sent a moment ago (by path, size and modification time):
/// an identical second request inside the window is not queued twice.
#[derive(Default)]
pub(crate) struct Recent(Vec<((String, u64, i64), std::time::Instant)>);

impl Recent {
    /// Whether `key` is new (and remembers it).
    pub(crate) fn fresh(&mut self, key: (String, u64, i64), now: std::time::Instant) -> bool {
        self.0
            .retain(|(_, at)| now.saturating_duration_since(*at) < Burst::WINDOW);
        if self.0.iter().any(|(seen, _)| *seen == key) {
            return false;
        }
        self.0.push((key, now));
        true
    }
}

/// Size and modification time (seconds), for [`Recent`].
pub(crate) fn fingerprint(path: &str) -> (u64, i64) {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).map_or((0, 0), |meta| (meta.len(), meta.mtime()))
}

/// The body of "Sending…" when the phone is not connected. The phone's name
/// is escaped: notification bodies are markup.
pub(crate) fn waiting_body(phone: &str) -> String {
    format!(
        "{} isn't connected. Your files are waiting (up to 200, for 24 hours) and will be sent when it connects.",
        glib::markup_escape_text(phone)
    )
}

pub(crate) fn too_many_message(entries: usize) -> String {
    // The count is not shown: the file manager's extension stops at 501.
    let _ = entries;
    format!("More than {MAX_ENTRIES} files selected. Send up to {MAX_ENTRIES} at a time.")
}

/// Says `body` where the person is looking: always on stderr (for a
/// terminal), and as a desktop notification when there is no terminal (a
/// file manager started us). Straight to the notification service on the
/// session bus, so it works whether or not DeskLink is running; a call that
/// fails is ignored (2 s at most).
pub(crate) fn tell(title: &str, body: &str) {
    eprintln!("desklink --send-files: {body}");
    if !std::io::stderr().is_terminal() {
        notify_desktop(title, body);
    }
}

fn notify_desktop(title: &str, body: &str) {
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return;
    };
    let hints =
        std::collections::HashMap::from([("desktop-entry".to_string(), APP_ID.to_variant())]);
    let call = (
        "DeskLink",
        0u32,
        APP_ID,
        title,
        body,
        Vec::<String>::new(),
        hints,
        8000i32,
    )
        .to_variant();
    let _ = bus.call_sync(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "Notify",
        Some(&call),
        None,
        gio::DBusCallFlags::NONE,
        2_000,
        gio::Cancellable::NONE,
    );
}

// ---- the integration files ------------------------------------------------

/// Phone ids are written unquoted into menus: only plain ones get an entry.
pub(crate) fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Format and line/paragraph separators (bidi controls, zero-width, U+2028/9):
/// invisible, and able to reorder what is around them.
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{0600}'..='\u{0605}' | '\u{061C}' | '\u{06DD}' | '\u{070F}'
        | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}' | '\u{FEFF}'
        | '\u{FFF9}'..='\u{FFFB}')
}

/// A phone's name as a menu label: no control or invisible characters,
/// trimmed, at most 80 bytes (a file name is 255 at most, with "Send to ", a
/// suffix and an extension around it).
pub(crate) fn clean_name(name: &str) -> String {
    let mut clean = String::new();
    for c in name
        .chars()
        .filter(|c| !c.is_control() && !is_invisible(*c))
    {
        if clean.len() + c.len_utf8() > 80 {
            break;
        }
        clean.push(c);
    }
    let clean = clean.trim().to_string();
    if clean.is_empty() {
        "phone".into()
    } else {
        clean
    }
}

/// `text` as a key-file value: a backslash would start an escape.
pub(crate) fn keyfile_value(text: &str) -> String {
    text.replace('\\', "\\\\")
}

/// The phones that get entries: plain ids only, cleaned names, one each.
pub(crate) fn clean_phones(phones: &[(String, String)]) -> Vec<(String, String)> {
    let mut clean: Vec<(String, String)> = Vec::new();
    for (id, name) in phones {
        if safe_id(id) && !clean.iter().any(|(seen, _)| seen == id) {
            clean.push((id.clone(), clean_name(name)));
        }
    }
    clean
}

/// Nemo's action for the only paired phone: a static file Nemo reads at menu
/// time, shown for files and folders (a folder is refused out loud).
pub(crate) fn nemo_action(exe: &Path, device_id: &str) -> String {
    format!(
        "[Nemo Action]\n# Written by DeskLink; removed when this phone is unpaired.\n{SCRIPT_MARK}{device_id}\nName=Send to phone\nComment=Send the selected files to your phone with DeskLink\nExec={} --send-files --to {device_id} -- %F\nIcon-Name=derx06.desklink.com-symbolic\nSelection=notnone\nExtensions=any;dir;\nQuote=double\nDependencies={}\n",
        shell_quote_for_nemo(exe),
        keyfile_value(&exe.to_string_lossy()),
    )
}

/// An `Exec` program as Nemo's command-line parser reads it (GLib shell
/// rules): single quotes, `'` as `'\''`; a literal `%` is a field code
/// otherwise, so `%%`; a backslash is doubled for the key file last.
fn shell_quote_for_nemo(exe: &Path) -> String {
    keyfile_value(&shell_quote(&exe.to_string_lossy()).replace('%', "%%"))
}

pub(crate) const EXTENSION_MARK: &str = "# desklink-send-to-extension";
/// In the phone list JSON and (as a line) the Dolphin menu.
pub(crate) const JSON_MARK: &str = "desklink-send-to";
pub(crate) const DOLPHIN_MARKS: [&str; 2] = [
    "X-DeskLink-Generated=send-to",
    // Written by older builds.
    "X-KDE-Submenu=Send to Phone",
];
pub(crate) const EXTENSION: &str = include_str!("desklink_send_to.py");

/// The JSON the Nautilus extension reads: the program and the phones.
pub(crate) fn phones_json(exe: &Path, phones: &[(String, String)]) -> String {
    serde_json::json!({
        "generator": JSON_MARK,
        "exe": exe.to_string_lossy(),
        "phones": phones.iter().map(|(id, name)| serde_json::json!({"id": id, "name": name})).collect::<Vec<_>>(),
    })
    .to_string()
}

/// Where each piece lives under the user's data and config directories.
pub(crate) struct Dirs {
    pub(crate) data: PathBuf,
    pub(crate) config: PathBuf,
}

impl Dirs {
    fn base(&self, base: Base) -> &Path {
        match base {
            Base::Data => &self.data,
            Base::Config => &self.config,
        }
    }
    fn extension(&self) -> PathBuf {
        self.data
            .join("nautilus-python/extensions/desklink_send_to.py")
    }
    fn phones(&self) -> PathBuf {
        self.data.join("desklink/send-to-phones.json")
    }
    fn nemo_actions(&self) -> PathBuf {
        self.data.join("nemo/actions")
    }
    fn service_menu(&self) -> PathBuf {
        self.data.join("kio/servicemenus").join(SERVICE_MENU)
    }
    fn thunar(&self) -> PathBuf {
        self.data.join("Thunar/sendto")
    }
}

/// What this computer has, as far as the file managers go.
pub(crate) struct Env<'a> {
    pub(crate) desktop: &'a str,
    pub(crate) installed: &'a dyn Fn(&str) -> bool,
    /// nautilus-python is installed (see [`python_nautilus_present`]).
    pub(crate) python_nautilus: bool,
}

fn remove_file(path: &Path, errors: &mut Vec<String>) {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            errors.push(format!("{}: {error}", path.display()))
        }
        _ => {}
    }
}

/// Removes `path` only when it carries one of `marks` (a file of ours).
fn remove_if_ours(path: &Path, marks: &[&str], errors: &mut Vec<String>) {
    if std::fs::read_to_string(path).is_ok_and(|text| marks.iter().any(|m| text.contains(m))) {
        remove_file(path, errors);
    }
}

/// Writes `content` atomically, but never over a file that is not ours (it
/// carries none of `marks`): that is left alone and reported.
pub(crate) fn write_marked(
    path: &Path,
    content: &str,
    executable: bool,
    marks: &[&str],
) -> std::io::Result<()> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if existing == content {
            return Ok(());
        }
        if !marks.iter().any(|m| existing.contains(m)) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{} is not a DeskLink file; left alone", path.display()),
            ));
        }
    } else if path.exists() {
        // Unreadable, a folder, a dangling link: not ours either.
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} is not a DeskLink file; left alone", path.display()),
        ));
    }
    write_atomic(path, content, executable)
}

/// A temporary file in the same folder, synced, then renamed over `path`.
pub(crate) fn write_atomic(path: &Path, content: &str, executable: bool) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".desklink-tmp-{}", std::process::id()));
    let mode = if executable { 0o755 } else { 0o644 };
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Brings every integration file in step: `enabled` and at least one phone
/// writes them where each file manager is used; otherwise every file this
/// feature wrote is removed. Returns the problems met (never stops at one).
pub(crate) fn sync(
    dirs: &Dirs,
    exe: &Path,
    phones: &[(String, String)],
    enabled: bool,
    env: &Env,
) -> Vec<String> {
    let mut errors = Vec::new();
    let phones = if enabled {
        clean_phones(phones)
    } else {
        Vec::new()
    };
    let any = !phones.is_empty();
    let nautilus = nautilus_wanted(env.desktop, (env.installed)("nautilus"));
    let nemo = nemo_wanted(env.desktop, env.installed);
    let extension = env.python_nautilus && nautilus && any;
    let single = phones.len() == 1;
    let nemo_action_in_use = nemo && single;
    let mut note = |what: &str, result: std::io::Result<()>| {
        if let Err(error) = result {
            errors.push(format!("{what}: {error}"));
        }
    };
    // Scripts (Nautilus, Nemo, Caja): where the top-level entry does not
    // replace them. The Bucket script follows the phones.
    let wanted_folders = script_folders(env.desktop, env.installed);
    for (base, rel) in [
        (Base::Data, "nautilus/scripts"),
        (Base::Data, "nemo/scripts"),
        (Base::Config, "caja/scripts"),
    ] {
        let dir = dirs.base(base).join(rel);
        let used = wanted_folders.contains(&(base, rel)) && any;
        let replaced = (rel == "nautilus/scripts" && extension)
            || (rel == "nemo/scripts" && nemo_action_in_use);
        let send: &[(String, String)] = if used && !replaced { &phones } else { &[] };
        note("Files scripts", sync_scripts(&dir, exe, send));
        let bucket = dir.join(BUCKET_SCRIPT);
        note(
            "Bucket script",
            if used {
                write_marked(&bucket, &bucket_script(exe), true, &[BUCKET_SCRIPT_MARK])
            } else if std::fs::read_to_string(&bucket).is_ok_and(|t| t.contains(BUCKET_SCRIPT_MARK))
            {
                std::fs::remove_file(&bucket)
            } else {
                Ok(())
            },
        );
    }
    // Nemo: one static action for the only phone.
    let action: &[(String, String)] = if nemo_action_in_use { &phones } else { &[] };
    note(
        "Nemo action",
        sync_per_phone(
            &dirs.nemo_actions(),
            action,
            ".nemo_action",
            false,
            |id, _| nemo_action(exe, id),
        ),
    );
    // Thunar's Send To.
    let thunar_wanted = has_desktop(env.desktop, &["XFCE"]) || (env.installed)("thunar");
    let thunar: &[(String, String)] = if thunar_wanted { &phones } else { &[] };
    note(
        "Thunar Send To",
        sync_per_phone(&dirs.thunar(), thunar, ".desktop", false, |id, name| {
            thunar_sendto(exe, id, name)
        }),
    );
    // Dolphin's service menu.
    let dolphin = crate::kde::is_kde(env.desktop) || (env.installed)("dolphin");
    let menu = dirs.service_menu();
    if dolphin && any {
        note(
            "Dolphin menu",
            write_marked(&menu, &dolphin_menu(exe, &phones), true, &DOLPHIN_MARKS),
        );
    } else {
        remove_if_ours(&menu, &DOLPHIN_MARKS, &mut errors);
    }
    // The Nautilus extension and its data.
    let (ext, json) = (dirs.extension(), dirs.phones());
    if extension {
        let mut note = |what: &str, result: std::io::Result<()>| {
            if let Err(error) = result {
                errors.push(format!("{what}: {error}"));
            }
        };
        note(
            "Nautilus extension",
            write_marked(&ext, EXTENSION, false, &[EXTENSION_MARK]),
        );
        note(
            "Phone list",
            write_marked(&json, &phones_json(exe, &phones), false, &[JSON_MARK]),
        );
    } else {
        remove_if_ours(&ext, &[EXTENSION_MARK], &mut errors);
        remove_if_ours(&json, &[JSON_MARK], &mut errors);
        if let Some(dir) = json.parent() {
            let _ = std::fs::remove_dir(dir); // only when empty
        }
    }
    errors
}

/// Everything this feature wrote, gone: the same as switching it off (and
/// `desklink --remove-integration`, for a package's post-removal hook).
pub(crate) fn remove_all(dirs: &Dirs) -> Vec<String> {
    let nothing = |_: &str| false;
    let mut errors = sync(
        dirs,
        Path::new("/"),
        &[],
        false,
        &Env {
            desktop: "",
            installed: &nothing,
            python_nautilus: false,
        },
    );
    // Every scripts folder, whatever desktop is running now.
    errors.extend(remove_open_with(&dirs.data.join("applications")));
    errors
}

/// The "Open With" helpers (ours only).
pub(crate) fn remove_open_with(applications: &Path) -> Vec<String> {
    let mut errors = Vec::new();
    for name in OPEN_WITH_FILES {
        let path = applications.join(name);
        if std::fs::read_to_string(&path).is_ok_and(|text| is_open_with_entry(&text)) {
            remove_file(&path, &mut errors);
        }
    }
    errors
}

/// `desklink --remove-integration`.
pub fn run_remove_cli() -> glib::ExitCode {
    let dirs = Dirs {
        data: glib::user_data_dir(),
        config: glib::user_config_dir(),
    };
    let errors = remove_all(&dirs);
    for error in &errors {
        eprintln!("desklink --remove-integration: {error}");
    }
    if errors.is_empty() {
        glib::ExitCode::SUCCESS
    } else {
        glib::ExitCode::FAILURE
    }
}

/// Whether nautilus-python (the loader Files needs for the extension) is
/// installed: its library in one of `dirs`, and Python can load the Nautilus
/// typelib (`probe`).
pub(crate) fn python_nautilus_present(dirs: &[PathBuf], probe: impl Fn() -> bool) -> bool {
    dirs.iter()
        .any(|dir| dir.join("libnautilus-python.so").is_file())
        && probe()
}

pub(crate) fn extension_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = ["/usr/lib", "/usr/lib64", "/usr/local/lib"]
        .iter()
        .map(|lib| PathBuf::from(lib).join("nautilus/extensions-4"))
        .collect();
    if let Ok(entries) = std::fs::read_dir("/usr/lib") {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().ends_with("-linux-gnu") {
                dirs.push(entry.path().join("nautilus/extensions-4"));
            }
        }
    }
    dirs
}

/// Can Python load the Nautilus 4 typelib (4.0 or 4.1)? A child process, so
/// call it off the UI thread.
pub(crate) fn python_loads_nautilus() -> bool {
    let code = "import gi\nok = False\nfor v in ('4.1', '4.0'):\n    try:\n        gi.require_version('Nautilus', v)\n        ok = True\n        break\n    except Exception:\n        pass\nraise SystemExit(0 if ok else 1)\n";
    std::process::Command::new("python3")
        .args(["-I", "-c", code])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("desklink-fm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn phones(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(id, name)| (id.to_string(), name.to_string()))
            .collect()
    }

    /// Every file under `dir`, relative.
    fn files(dir: &Path) -> Vec<String> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(base, &path, out);
                } else {
                    out.push(
                        path.strip_prefix(base)
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    const HOSTILE: [&str; 7] = [
        "$(rm -rf ~)",
        "\"; touch x; \"",
        "a\nExec=evil --boom\n[Desktop Action x]",
        "back\\slash\\",
        "`id` %f %F ' \"",
        "Pixel \u{1F4F1} \u{00e9}",
        "   ",
    ];

    // ---- S1: refusals ----

    #[test]
    fn every_refusal_is_worded() {
        let skip = |f| Skipped {
            folders: f,
            ..Skipped::default()
        };
        assert_eq!(skip(0).message(), None);
        assert_eq!(
            skip(2).message().as_deref(),
            Some("Folders aren't sent yet. Select files.")
        );
        let mixed = Skipped {
            folders: 1,
            remote: 2,
            missing: 1,
            unreadable: 3,
            special: 1,
            protected: 1,
            badname: 1,
        };
        let text = mixed.message().unwrap();
        for part in [
            "Folders aren't sent yet.",
            "2 items aren't stored on this computer",
            "1 file no longer exist",
            "3 files can't be read",
            "1 item aren't ordinary",
            "DeskLink's own files",
        ] {
            assert!(text.contains(part), "{part} in {text}");
        }
        assert!(too_many_message(900).starts_with("More than 500 files"));
        assert_eq!(
            Skipped {
                badname: 2,
                ..Skipped::default()
            }
            .message()
            .as_deref(),
            Some("2 file names are not valid text and can't be sent.")
        );
    }

    #[test]
    fn the_command_line_resolves_uris_and_counts_what_it_cannot() {
        let cwd = Path::new("/home/me");
        let args: Vec<String> = [
            "file:///tmp/a%20b%27c.txt",
            "smb://host/share/x",
            "trash:///old",
            "recent:///",
            "/tmp/odd://name",
            "rel.txt",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let request = parse(&args, cwd);
        assert_eq!(request.remote, 3);
        assert_eq!(
            request.paths,
            [
                PathBuf::from("/tmp/a b'c.txt"),
                PathBuf::from("/tmp/odd://name"),
                PathBuf::from("/home/me/rel.txt"),
            ]
        );
        assert!(
            is_uri("smb://x") && is_uri("file:///x") && !is_uri("/a://b") && !is_uri("a b://c")
        );
    }

    // ---- S2: validation ----

    #[test]
    fn a_request_is_checked_again_at_action_time() {
        let dir = tmp();
        let file = dir.join("it's \"a\" $file\n.txt");
        std::fs::write(&file, "x").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let loop_a = dir.join("loop-a");
        std::os::unix::fs::symlink(dir.join("loop-b"), &loop_a).unwrap();
        std::os::unix::fs::symlink(&loop_a, dir.join("loop-b")).unwrap();
        let sub = dir.join("folder");
        std::fs::create_dir(&sub).unwrap();
        let socket = dir.join("sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let locked = dir.join("locked");
        std::fs::write(&locked, "x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let can_open_locked = std::fs::File::open(&locked).is_ok(); // root

        let (ok, skipped) = classify(&[
            file.clone(),
            link.clone(),
            sub,
            socket,
            dir.join("gone"),
            loop_a,
            PathBuf::from("relative.txt"),
            locked,
            PathBuf::from("/dev/null"),
        ]);
        assert_eq!(
            ok.len(),
            if can_open_locked { 3 } else { 2 },
            "the file and its symlink: {ok:?}"
        );
        assert_eq!(ok[0], file.to_string_lossy());
        assert_eq!(skipped.folders, 1);
        assert_eq!(skipped.special, 2, "a socket and /dev/null");
        assert_eq!(skipped.missing, 3, "gone, a symlink loop, a relative path");
        assert_eq!(skipped.unreadable, usize::from(!can_open_locked));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn desklinks_own_state_is_never_sent() {
        let own = glib::user_config_dir().join("desklink");
        std::fs::create_dir_all(&own).unwrap();
        let key = own.join("privateKey.pem");
        std::fs::write(&key, "secret").unwrap();
        let dir = tmp();
        let sneaky = dir.join("sneaky");
        std::os::unix::fs::symlink(&key, &sneaky).unwrap();
        let (ok, skipped) = classify(&[key.clone(), sneaky]);
        assert!(ok.is_empty(), "{ok:?}");
        assert_eq!(skipped.protected, 2);
        std::fs::remove_file(&key).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_burst_of_requests_is_cut_off() {
        use std::time::{Duration, Instant};
        let (mut burst, t0) = (Burst::default(), Instant::now());
        for n in 0..Burst::MAX as u64 {
            assert!(burst.allow(t0 + Duration::from_millis(n)), "{n}");
        }
        assert!(!burst.allow(t0 + Duration::from_secs(5)), "a sixth in 10 s");
        assert!(
            burst.allow(t0 + Duration::from_secs(11)),
            "the window moved on"
        );
    }

    // ---- S3: generated files ----

    fn run_script(args: &[&str]) -> Vec<String> {
        let dir = tmp();
        let out = dir.join("args");
        let stub = dir.join("a stub's \"desklink\" $HOME");
        std::fs::write(
            &stub,
            format!("#!/bin/sh\nprintf '%s\\0' \"$@\" > '{}'\n", out.display()),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.join("script");
        std::fs::write(&path, super::super::script(&stub, "p1")).unwrap();
        let status = std::process::Command::new("sh")
            .arg(&path)
            .args(args)
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(status.success());
        let raw = std::fs::read(&out).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        raw.split(|b| *b == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect()
    }

    #[test]
    fn the_files_script_passes_hostile_names_as_plain_arguments() {
        let names = [
            "it's \"q\" $(touch pwned) `id` %F;&|",
            "line\nbreak.txt",
            "-rf",
            "\u{e9}\u{1F4F1}.txt",
        ];
        let got = run_script(&names);
        assert_eq!(&got[..4], ["--send-files", "--to", "p1", "--"]);
        assert_eq!(&got[4..], names);
        assert!(!Path::new("pwned").exists());
    }

    fn keyfile(text: &str) -> glib::KeyFile {
        let file = glib::KeyFile::new();
        file.load_from_data(text, glib::KeyFileFlags::NONE)
            .unwrap_or_else(|e| panic!("not a valid key file ({e}):\n{text}"));
        file
    }

    #[test]
    fn menus_survive_hostile_phone_names_and_exe_paths() {
        let exe = Path::new("/opt/my \"apps\"/$HOME/100%/de`sk\\link");
        let list: Vec<(String, String)> = HOSTILE
            .iter()
            .enumerate()
            .map(|(i, name)| (format!("p{i}"), name.to_string()))
            .chain([
                ("bad id; touch x".to_string(), "Evil".to_string()),
                ("ok".to_string(), "Fine".to_string()),
                ("new\nline".to_string(), "Nl".to_string()),
                ("a b".to_string(), "Sp".to_string()),
            ])
            .collect();
        let clean = clean_phones(&list);
        assert_eq!(clean.len(), HOSTILE.len() + 1, "only plain ids: {clean:?}");
        assert!(clean
            .iter()
            .all(|(id, name)| safe_id(id) && !name.contains(['\n', '\r'])));
        assert_eq!(clean.last().unwrap().1, "Fine");
        assert_eq!(clean[HOSTILE.len() - 1].1, "phone", "a blank name");
        // Dolphin
        let dolphin = keyfile(&super::super::dolphin_menu(exe, &clean));
        let group = "Desktop Action phone0";
        let argv = glib::shell_parse_argv(dolphin.string(group, "Exec").unwrap().as_str()).unwrap();
        let argv: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // The Desktop Entry layer doubles a literal `%` (`%%`); the shell layer is exact.
        let spec_exe = exe.to_str().unwrap().replace('%', "%%");
        assert_eq!(
            argv,
            [spec_exe.as_str(), "--send-files", "--to", "p0", "--", "%F"]
        );
        assert_eq!(
            dolphin.string(group, "Name").unwrap(),
            "Send to $(rm -rf ~)"
        );
        for i in 0..clean.len() {
            let name = dolphin
                .string(&format!("Desktop Action phone{i}"), "Name")
                .unwrap();
            assert!(name.starts_with("Send to "), "{name}");
            assert_eq!(
                dolphin
                    .keys(&format!("Desktop Action phone{i}"))
                    .unwrap()
                    .len(),
                3
            );
        }
        assert_eq!(
            dolphin.groups().len(),
            clean.len() + 1,
            "no injected groups"
        );
        // Thunar
        for (id, name) in &clean {
            let entry = keyfile(&super::super::thunar_sendto(exe, id, name));
            assert_eq!(entry.groups().len(), 1, "no injected group");
            let mut keys: Vec<String> = entry
                .keys("Desktop Entry")
                .unwrap()
                .iter()
                .map(|k| k.to_string())
                .collect();
            keys.sort();
            assert_eq!(
                keys,
                ["Exec", "Icon", "Name", "Type", "Version"],
                "no injected key"
            );
            let argv =
                glib::shell_parse_argv(entry.string("Desktop Entry", "Exec").unwrap().as_str())
                    .unwrap();
            let argv: Vec<String> = argv
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let spec_exe = exe.to_str().unwrap().replace('%', "%%");
            assert_eq!(
                argv,
                [
                    spec_exe.as_str(),
                    "--send-files",
                    "--to",
                    id.as_str(),
                    "--",
                    "%F"
                ]
            );
        }
        // Nemo
        let action = nemo_action(exe, "p0");
        let nemo = keyfile(&action);
        let argv =
            glib::shell_parse_argv(nemo.string("Nemo Action", "Exec").unwrap().as_str()).unwrap();
        let argv: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            [spec_exe.as_str(), "--send-files", "--to", "p0", "--", "%F"]
        );
        assert_eq!(
            nemo.string("Nemo Action", "Dependencies").unwrap(),
            exe.to_str().unwrap()
        );
        // JSON is data, not code.
        let json: serde_json::Value = serde_json::from_str(&phones_json(exe, &clean)).unwrap();
        assert_eq!(json["exe"], exe.to_str().unwrap());
        assert_eq!(json["phones"][0]["name"], "$(rm -rf ~)");
    }

    #[test]
    fn the_dolphin_menu_shows_for_files_and_folders() {
        let menu =
            super::super::dolphin_menu(Path::new("/usr/bin/desklink"), &phones(&[("p1", "P")]));
        assert!(menu.contains("MimeType=application/octet-stream;inode/directory;\n"));
        assert!(menu.contains("X-KDE-Protocols=file\n"));
    }

    fn setup() -> (Dirs, PathBuf) {
        let root = tmp();
        let dirs = Dirs {
            data: root.join("data"),
            config: root.join("config"),
        };
        (dirs, root)
    }

    fn env<'a>(desktop: &'a str, python: bool, installed: &'a dyn Fn(&str) -> bool) -> Env<'a> {
        Env {
            desktop,
            installed,
            python_nautilus: python,
        }
    }

    const EXE: &str = "/usr/bin/desklink";

    #[test]
    fn nautilus_gets_the_extension_when_python_nautilus_is_there_scripts_when_not() {
        let (dirs, root) = setup();
        let none = |_: &str| false;
        let one = phones(&[("p1", "Pixel")]);
        // No python-nautilus: scripts (the fallback), no extension.
        assert!(sync(
            &dirs,
            Path::new(EXE),
            &one,
            true,
            &env("GNOME", false, &none)
        )
        .is_empty());
        let got = files(&root);
        assert!(
            got.contains(&"data/nautilus/scripts/Send to Pixel".into()),
            "{got:?}"
        );
        assert!(got.contains(&"data/nautilus/scripts/Add to DeskLink Bucket".into()));
        assert!(!got.iter().any(|f| f.contains("nautilus-python")));
        // With it: the extension and its data; the Send script goes, Bucket stays.
        assert!(sync(
            &dirs,
            Path::new(EXE),
            &one,
            true,
            &env("GNOME", true, &none)
        )
        .is_empty());
        let got = files(&root);
        assert_eq!(
            got,
            [
                "data/desklink/send-to-phones.json",
                "data/nautilus-python/extensions/desklink_send_to.py",
                "data/nautilus/scripts/Add to DeskLink Bucket",
            ]
        );
        // Switch off: every file this feature wrote is gone.
        assert!(sync(
            &dirs,
            Path::new(EXE),
            &one,
            false,
            &env("GNOME", true, &none)
        )
        .is_empty());
        assert_eq!(files(&root), Vec::<String>::new());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn nemo_uses_a_static_action_for_one_phone_and_scripts_for_several() {
        let (dirs, root) = setup();
        let none = |_: &str| false;
        let e = env("X-Cinnamon", false, &none);
        sync(&dirs, Path::new(EXE), &phones(&[("p1", "Pixel")]), true, &e);
        assert_eq!(
            files(&root),
            [
                "data/nemo/actions/Send to Pixel.nemo_action",
                "data/nemo/scripts/Add to DeskLink Bucket",
            ]
        );
        sync(
            &dirs,
            Path::new(EXE),
            &phones(&[("p1", "Pixel"), ("p2", "Tab")]),
            true,
            &e,
        );
        let got = files(&root);
        assert!(
            got.contains(&"data/nemo/scripts/Send to Tab".into()),
            "{got:?}"
        );
        assert!(!got.iter().any(|f| f.contains("actions")), "{got:?}");
        sync(&dirs, Path::new(EXE), &[], true, &e);
        assert_eq!(files(&root), Vec::<String>::new(), "last phone unpaired");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn remove_all_clears_every_manager_and_leaves_the_users_own_files() {
        let (dirs, root) = setup();
        let all = |p: &str| ["nemo", "dolphin", "thunar", "nautilus", "caja"].contains(&p);
        let two = phones(&[("p1", "Pixel"), ("p2", "Tab")]);
        let e = env("GNOME:KDE", true, &all);
        assert!(sync(&dirs, Path::new(EXE), &two, true, &e).is_empty());
        let before = files(&root);
        for expect in [
            "kio/servicemenus",
            "Thunar/sendto",
            "nemo/scripts",
            "caja/scripts",
            "nautilus-python",
        ] {
            assert!(
                before.iter().any(|f| f.contains(expect)),
                "{expect}: {before:?}"
            );
        }
        std::fs::create_dir_all(dirs.data.join("nautilus/scripts")).unwrap();
        std::fs::write(dirs.data.join("nautilus/scripts/Mine"), "#!/bin/sh\n").unwrap();
        std::fs::write(
            dirs.data.join("nautilus-python/extensions/other.py"),
            "x = 1\n",
        )
        .unwrap();
        std::fs::create_dir_all(dirs.data.join("applications")).unwrap();
        for name in OPEN_WITH_FILES {
            std::fs::write(
                dirs.data.join("applications").join(name),
                super::super::open_with_helper(
                    "N",
                    "C",
                    Path::new(EXE),
                    "--bucket",
                    &["image/png".into()],
                ),
            )
            .unwrap();
        }
        assert!(remove_all(&dirs).is_empty());
        assert_eq!(
            files(&root),
            [
                "data/nautilus-python/extensions/other.py",
                "data/nautilus/scripts/Mine"
            ],
            "only the user's own files remain"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_hostile_phone_gets_no_file_anywhere() {
        let (dirs, root) = setup();
        let all = |_: &str| true;
        let evil = phones(&[("x; touch pwned", "Evil"), ("ok_1", "$(touch pwned)")]);
        let e = env("GNOME:KDE:X-Cinnamon:XFCE", true, &all);
        assert!(sync(&dirs, Path::new(EXE), &evil, true, &e).is_empty());
        assert_eq!(
            files(&root),
            [
                "config/caja/scripts/Add to DeskLink Bucket",
                "config/caja/scripts/Send to $(touch pwned)",
                "data/Thunar/sendto/Send to $(touch pwned).desktop",
                "data/desklink/send-to-phones.json",
                "data/kio/servicemenus/derx06.desklink.com-send.desktop",
                "data/nautilus-python/extensions/desklink_send_to.py",
                "data/nautilus/scripts/Add to DeskLink Bucket",
                "data/nemo/actions/Send to $(touch pwned).nemo_action",
                "data/nemo/scripts/Add to DeskLink Bucket",
            ],
            "only the plain-id phone, and its odd name is only ever text"
        );
        for file in files(&root) {
            let text = std::fs::read_to_string(root.join(&file)).unwrap();
            assert!(!text.contains("x; touch"), "{file}");
        }
        assert!(!Path::new("pwned").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_users_own_file_at_a_fixed_name_is_neither_overwritten_nor_deleted() {
        let (dirs, root) = setup();
        let all = |_: &str| true;
        let e = env("GNOME:KDE:X-Cinnamon:XFCE", true, &all);
        let theirs = "# my own file, nothing of DeskLink's\n[Desktop Entry]\nName=Mine\n";
        let two = phones(&[("p1", "Pixel"), ("p2", "Tab")]);
        let one = phones(&[("p1", "Pixel")]);
        let spots = [
            dirs.data
                .join("nautilus-python/extensions/desklink_send_to.py"),
            dirs.data.join("desklink/send-to-phones.json"),
            dirs.data.join("kio/servicemenus").join(SERVICE_MENU),
            dirs.data.join("nautilus/scripts").join(BUCKET_SCRIPT),
            dirs.config.join("caja/scripts/Send to Pixel"),
            dirs.data.join("nemo/actions/Send to Pixel.nemo_action"),
            dirs.data.join("Thunar/sendto/Send to Pixel.desktop"),
        ];
        for spot in &spots {
            std::fs::create_dir_all(spot.parent().unwrap()).unwrap();
            std::fs::write(spot, theirs).unwrap();
        }
        // One phone (extension, nemo action) and two (scripts): every spot is hit.
        for list in [&one, &two] {
            let errors = sync(&dirs, Path::new(EXE), list, true, &e);
            assert!(!errors.is_empty(), "said, not silent");
            for spot in &spots {
                assert_eq!(std::fs::read_to_string(spot).unwrap(), theirs, "{spot:?}");
            }
        }
        // The phones' other entries were still written: one failure stops nothing.
        assert!(dirs.config.join("caja/scripts/Send to Tab").exists());
        // Switched off, and removed outright: still there.
        sync(&dirs, Path::new(EXE), &one, false, &e);
        remove_all(&dirs);
        for spot in &spots {
            assert_eq!(std::fs::read_to_string(spot).unwrap(), theirs, "{spot:?}");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_menu_an_older_build_wrote_is_still_ours() {
        let (dirs, root) = setup();
        let menu = dirs.data.join("kio/servicemenus").join(SERVICE_MENU);
        std::fs::create_dir_all(menu.parent().unwrap()).unwrap();
        std::fs::write(
            &menu,
            "[Desktop Entry]\nType=Service\nMimeType=application/octet-stream;\nActions=phone0;\nX-KDE-Priority=TopLevel\nX-KDE-Submenu=Send to Phone\n",
        )
        .unwrap();
        let kde = |p: &str| p == "dolphin";
        let one = phones(&[("p1", "Pixel")]);
        assert!(sync(&dirs, Path::new(EXE), &one, true, &env("KDE", false, &kde)).is_empty());
        assert!(std::fs::read_to_string(&menu)
            .unwrap()
            .contains("X-DeskLink-Generated=send-to"));
        sync(&dirs, Path::new(EXE), &one, false, &env("KDE", false, &kde));
        assert!(!menu.exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn writes_are_atomic_and_leave_no_temporary_file() {
        let dir = tmp();
        let file = dir.join("sub/entry");
        write_atomic(&file, "one", true).unwrap();
        write_atomic(&file, "two", true).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let plain = dir.join("sub/plain");
        write_atomic(&plain, "x", false).unwrap();
        assert_eq!(
            std::fs::metadata(&plain).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(files(&dir), ["sub/entry", "sub/plain"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_phone_name_is_escaped_in_a_notification_and_cannot_stop_the_others() {
        let body = waiting_body("<a href=\"https://evil\">x</a> & co");
        assert!(!body.contains('<') && body.contains("&lt;a href"), "{body}");
        assert!(body.contains("200") && body.contains("24 hours"));
        // 60 four-byte emoji, and a very long plain name.
        let emoji = "\u{1F4F1}".repeat(60);
        assert!(clean_name(&emoji).len() <= 80);
        assert!(clean_name(&"a".repeat(400)).len() <= 80);
        let (dirs, root) = setup();
        let none = |_: &str| false;
        let list = phones(&[("p1", emoji.as_str()), ("p2", "Tab"), ("p3", "Pixel")]);
        let errors = sync(
            &dirs,
            Path::new(EXE),
            &list,
            true,
            &env("GNOME", false, &none),
        );
        assert!(errors.is_empty(), "{errors:?}");
        let got = files(&root);
        assert!(got.iter().any(|f| f.ends_with("Send to Tab")), "{got:?}");
        assert!(got.iter().any(|f| f.ends_with("Send to Pixel")), "{got:?}");
        assert_eq!(got.len(), 4, "three phones and the Bucket: {got:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn invisible_and_bidi_characters_are_dropped_from_names() {
        assert_eq!(
            clean_name("a\u{202E}b\u{200B}c\u{2028}d\u{FEFF}\u{2066}e"),
            "abcde"
        );
        assert_eq!(clean_name("\u{200B}\u{202E}"), "phone");
    }

    #[test]
    fn an_identical_request_inside_the_window_is_not_queued_twice() {
        use std::time::{Duration, Instant};
        let (mut recent, t0) = (Recent::default(), Instant::now());
        let key = ("/a/b".to_string(), 10, 5);
        assert!(recent.fresh(key.clone(), t0));
        assert!(!recent.fresh(key.clone(), t0 + Duration::from_secs(3)));
        assert!(
            recent.fresh(("/a/b".into(), 11, 5), t0 + Duration::from_secs(3)),
            "changed size"
        );
        assert!(
            recent.fresh(key, t0 + Duration::from_secs(11)),
            "window over"
        );
    }

    #[test]
    fn a_name_that_is_not_text_and_a_hard_link_to_our_key_are_refused() {
        let own = glib::user_config_dir().join("desklink");
        std::fs::create_dir_all(&own).unwrap();
        let key = own.join("hardlinked.pem");
        std::fs::write(&key, "secret").unwrap();
        let dir = tmp();
        let link = dir.join("innocent.txt");
        std::fs::hard_link(&key, &link).unwrap();
        let (ok, skipped) = classify(&[link, PathBuf::from("/tmp/bad\u{FFFD}name")]);
        assert!(ok.is_empty(), "{ok:?}");
        assert_eq!(
            (skipped.protected, skipped.badname, skipped.missing),
            (1, 1, 0)
        );
        std::fs::remove_file(&key).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nautilus_python_is_found_by_its_library_and_a_working_typelib() {
        let dir = tmp();
        assert!(
            !python_nautilus_present(std::slice::from_ref(&dir), || true),
            "no library"
        );
        std::fs::write(dir.join("libnautilus-python.so"), "").unwrap();
        assert!(python_nautilus_present(std::slice::from_ref(&dir), || true));
        assert!(
            !python_nautilus_present(std::slice::from_ref(&dir), || false),
            "no typelib"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- the Python extension ----

    fn have_python() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[test]
    fn the_extension_compiles() {
        if !have_python() {
            return;
        }
        let dir = tmp();
        let path = dir.join("desklink_send_to.py");
        std::fs::write(&path, EXTENSION).unwrap();
        let out = std::process::Command::new("python3")
            .args(["-I", "-m", "py_compile"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(EXTENSION.starts_with(EXTENSION_MARK));
        std::fs::remove_dir_all(&dir).ok();
    }

    const HARNESS: &str = r#"
import importlib.util, json, os, sys, time, types

class _Item:
    def __init__(self, **kw):
        self.kw = kw; self.handlers = []; self.sub = None
    def connect(self, sig, cb): self.handlers.append(cb)
    def set_submenu(self, menu): self.sub = menu
    def activate(self):
        for cb in self.handlers: cb(self)
class _Menu:
    def __init__(self): self.items = []
    def append_item(self, item): self.items.append(item)
class _Provider: pass
class _GObj: pass
nautilus = types.SimpleNamespace(MenuItem=_Item, Menu=_Menu, MenuProvider=_Provider)
gobject = types.SimpleNamespace(GObject=_GObj)
gi = types.ModuleType("gi"); gi.require_version = lambda *a: None
repo = types.ModuleType("gi.repository"); repo.Nautilus = nautilus; repo.GObject = gobject
gi.repository = repo
sys.modules["gi"] = gi; sys.modules["gi.repository"] = repo

home, ext, exe, out = sys.argv[1:5]
os.environ["XDG_DATA_HOME"] = home
spec = importlib.util.spec_from_file_location("ext", ext)
mod = importlib.util.module_from_spec(spec); spec.loader.exec_module(mod)

class F:
    def __init__(self, path, scheme="file", folder=False):
        self.path, self.scheme, self.folder = path, scheme, folder
    def get_uri_scheme(self): return self.scheme
    def get_uri(self): return "%s://%s" % (self.scheme, self.path or "x")
    def is_directory(self): return self.folder
    def get_location(self): return types.SimpleNamespace(get_path=lambda: self.path)

provider = mod.SendToPhone()
def write(phones, program=exe):
    os.makedirs(os.path.join(home, "desklink"), exist_ok=True)
    with open(os.path.join(home, "desklink", "send-to-phones.json"), "w") as h:
        json.dump({"exe": program, "phones": [{"id": i, "name": n} for i, n in phones]}, h)

# no data file: nothing, no exception
assert provider.get_file_items([F("/a")]) == []
write([("p1", "Pixel")])
files = [F("/tmp/it's \"x\" $(id)\n.txt"), F("/tmp/dir", folder=True), F("/x", scheme="smb"), F(None)]
items = provider.get_file_items(files)
assert len(items) == 1 and items[0].kw["label"] == "Send to phone", items
assert items[0].sub is None
assert provider.get_file_items([F("/tmp/dir", folder=True)]) == [], "folders only: no entry"
assert provider.get_file_items([F("/x", scheme="trash")]) == []
assert provider.get_file_items(None) == [], "never raises"
assert provider.get_file_items(object(), files) != [], "older (window, files) form"
items[0].activate()
for _ in range(100):
    if os.path.exists(out): break
    time.sleep(0.05)
time.sleep(0.2)
got = open(out, "rb").read().split(b"\0")[:-1]
assert got == [b"--send-files", b"--to", b"p1", b"--", "/tmp/it's \"x\" $(id)\n.txt".encode(), b"/tmp/dir", b"smb:///x", b"file://x"], got
# several phones: one entry, a submenu
write([("p1", "Pixel"), ("p2", "$(rm -rf ~) \"Tab\"")])
items = provider.get_file_items(files)
assert len(items) == 1 and items[0].sub is not None and len(items[0].sub.items) == 2
assert items[0].sub.items[1].kw["label"] == "Send to $(rm -rf ~) \"Tab\"", "no _ in it"
write([("p1", "Pixel"), ("p2", "my_phone")])
assert provider.get_file_items(files)[0].sub.items[1].kw["label"] == "Send to my__phone", "mnemonic escaped"
# a broken or hostile data file: nothing
for bad in ["not json", "{}", json.dumps({"exe": "relative", "phones": []}), json.dumps({"exe": exe, "phones": [{"id": 1}]})]:
    with open(os.path.join(home, "desklink", "send-to-phones.json"), "w") as h: h.write(bad)
    assert provider.get_file_items(files) == [], bad
print("OK")
"#;

    #[test]
    fn the_extension_builds_its_menu_against_a_stub_nautilus() {
        if !have_python() {
            return;
        }
        let dir = tmp();
        let ext = dir.join("desklink_send_to.py");
        std::fs::write(&ext, EXTENSION).unwrap();
        let harness = dir.join("harness.py");
        std::fs::write(&harness, HARNESS).unwrap();
        let out = dir.join("recorded");
        let exe = dir.join("my desklink");
        std::fs::write(
            &exe,
            format!("#!/bin/sh\nprintf '%s\\0' \"$@\" > '{}'\n", out.display()),
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let run = std::process::Command::new("python3")
            .arg("-I")
            .arg(&harness)
            .arg(dir.join("data"))
            .arg(&ext)
            .arg(&exe)
            .arg(&out)
            .output()
            .unwrap();
        assert!(
            run.status.success() && String::from_utf8_lossy(&run.stdout).contains("OK"),
            "{}\n{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
