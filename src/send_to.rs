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
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        eprintln!("desklink --send-files: no session bus");
        return glib::ExitCode::FAILURE;
    };
    let parameter = (request.device.unwrap_or_default(), paths).to_variant();
    let call = || {
        bus.call_sync(
            Some(APP_ID),
            APP_PATH,
            "org.gtk.Actions",
            "Activate",
            Some(
                &(
                    "send-files",
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
    eprintln!("desklink --send-files: DeskLink did not start");
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

fn send(app: &DesklinkApplication, device: &str, paths: &[String]) {
    let Some(daemon) = app.daemon_handle() else {
        return;
    };
    let devices = daemon.devices();
    let target = devices
        .iter()
        .filter(|candidate| candidate.trusted)
        .find(|candidate| candidate.id == device)
        .or_else(|| {
            devices
                .iter()
                .find(|candidate| candidate.trusted && candidate.feature_transport_ready)
        })
        .or_else(|| devices.iter().find(|candidate| candidate.trusted));
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
pub(crate) fn open_with_entry(exe: &Path) -> String {
    let exe: String = exe
        .to_string_lossy()
        .chars()
        .flat_map(|c| match c {
            '"' | '`' | '$' | '\\' => vec!['\\', c],
            c => vec![c],
        })
        .collect();
    format!(
        "[Desktop Entry]\nType=Application\nName=Send to Phone\nComment=Send files to your phone with DeskLink\nIcon=derx06.desklink.com\nExec=\"{exe}\" --send-files %F\nTerminal=false\nNoDisplay=true\nMimeType={MIME_TYPES}\n"
    )
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
    let scripts = data.join("nautilus").join("scripts");
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
            if let Err(error) = sync_scripts(&scripts, &exe, &phones) {
                eprintln!("[DeskLink] Could not update the Files scripts: {error}");
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
        assert!(entry.contains("Exec=\"/home/me/my \\\"bin\\\"/desklink\" --send-files %F\n"));
        assert!(entry.contains("MimeType=application/octet-stream;"));
        assert!(entry.contains("NoDisplay=true"));
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
