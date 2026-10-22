//! "Send to <phone>" from Files: `desklink --send-files [--to <device>]
//! <paths…>`, a Nautilus script per paired phone, and an Open With entry.
//!
//! The command runs before GTK starts: it asks the running DeskLink (over
//! its own D-Bus name, the `app.send-files` action) to send the files,
//! starting it first if it is not running. The scripts and the `.desktop`
//! entry are written by the app itself, under the user's data dir: nothing
//! to install. A script is removed when its phone is unpaired.

use std::path::{Path, PathBuf};

use gtk::gio;
use gtk::glib::{self, Variant};
use gtk::prelude::*;

use crate::application::DesklinkApplication;

const APP_ID: &str = "derx06.desklink.com";
const APP_PATH: &str = "/derx06/desklink/com";
/// Marks a script DeskLink wrote, with the phone it is for.
const SCRIPT_MARK: &str = "# desklink-device: ";
const OPEN_WITH: &str = "derx06.desklink.com.SendTo.desktop";
/// F8b: "Add to DeskLink Bucket" in Open With.
const BUCKET_OPEN_WITH: &str = "derx06.desklink.com.Bucket.desktop";
/// The Files script that puts the chosen files in the Bucket.
const BUCKET_SCRIPT: &str = "Add to DeskLink Bucket";
const BUCKET_SCRIPT_MARK: &str = "# desklink-bucket";
/// File types the Open With entry is offered for; `application/octet-stream`
/// puts it under "Open With…" for every file.
const MIME_TYPES: &str = "application/octet-stream;image/jpeg;image/png;image/gif;image/webp;\
image/heic;video/mp4;video/quicktime;video/webm;audio/mpeg;audio/ogg;audio/flac;audio/x-wav;\
application/pdf;text/plain;application/zip;";

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
            let (files, folder) = crate::device_links::folders::split_files(paths);
            let count = files.len();
            if count > 0 {
                crate::device_links::folders::import(&device, files);
            }
            let notification = gio::Notification::new(&match count {
                0 => "Nothing was added to the Bucket".to_string(),
                1 => "Added to the Bucket".to_string(),
                count => format!("Added {count} files to the Bucket"),
            });
            if folder {
                notification.set_body(Some(crate::device_links::folders::FOLDERS_NOT_YET));
            }
            notification
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
                notification.set_body(Some("It starts as soon as the phone is connected."));
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

/// The Open With entry: `Exec` quoted per the Desktop Entry spec.
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

/// "Add to DeskLink Bucket" in Open With.
pub(crate) fn bucket_open_with_entry(exe: &Path) -> String {
    let exe = desktop_exec_quoted(exe);
    format!(
        "[Desktop Entry]\nType=Application\nName=Add to DeskLink Bucket\nComment=Put files in the Bucket you share with your phone\nIcon=derx06.desklink.com\nExec={exe} --bucket %F\nTerminal=false\nNoDisplay=true\nMimeType={MIME_TYPES}\n"
    )
}

pub(crate) fn open_with_entry(exe: &Path) -> String {
    let exe = desktop_exec_quoted(exe);
    format!(
        "[Desktop Entry]\nType=Application\nName=Send to Phone\nComment=Send files to your phone with DeskLink\nIcon=derx06.desklink.com\nExec={exe} --send-files %F\nTerminal=false\nNoDisplay=true\nMimeType={MIME_TYPES}\n"
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

/// Keeps the scripts and the Open With entry in step with the paired phones
/// while the app runs.
pub fn start(app: &DesklinkApplication) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let data = glib::user_data_dir();
    if let Err(error) = write_if_changed(
        &data.join("applications").join(OPEN_WITH),
        &open_with_entry(&exe),
        false,
    ) {
        eprintln!("[DeskLink] Could not add \"Send to Phone\" to Open With: {error}");
    }
    let _ = write_if_changed(
        &data.join("applications").join(BUCKET_OPEN_WITH),
        &bucket_open_with_entry(&exe),
        false,
    );
    let scripts = data.join("nautilus").join("scripts");
    let service_menu = data.join("kio").join("servicemenus").join(SERVICE_MENU);
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    // Each file manager's extra only where it is used.
    let nautilus = nautilus_wanted(&desktop, on_path("nautilus"));
    let dolphin = crate::kde::is_kde(&desktop) || on_path("dolphin");
    // The paired phones the scripts were last written for.
    let last = std::cell::RefCell::new(None::<Vec<(String, String)>>);
    let weak = app.downgrade();
    glib::timeout_add_seconds_local(2, move || {
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
    fn the_open_with_entry_quotes_its_command() {
        let entry = open_with_entry(Path::new("/home/me/my \"bin\"/desklink"));
        assert!(
            entry.contains("Exec=\"/home/me/my \\\\\"bin\\\\\"/desklink\" --send-files %F\n"),
            "{entry}"
        );
        assert_eq!(
            desktop_exec_quoted(Path::new("/a\\b/$c/100%/d`e")),
            "\"/a\\\\\\\\b/\\\\$c/100%%/d\\\\`e\"",
            "a backslash is four, a dollar two plus itself, a percent doubled"
        );
        assert!(entry.contains("MimeType=application/octet-stream;"));
        assert!(entry.contains("NoDisplay=true"));
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
}
