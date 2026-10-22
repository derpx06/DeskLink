//! GNOME Shell search: `org.gnome.Shell.SearchProvider2` at
//! `/derx06/desklink/com/SearchProvider`. Typing in the Activities overview
//! finds this computer's shared terminals (open one at the desk), the run
//! commands (run one here), recent transfers (open the file), the phones
//! (ring one) and the contacts they share (show one).
//!
//! GNOME reads providers from `gnome-shell/search-providers/*.ini` under the
//! data dirs. Packages install `data/derx06.desklink.com.search-provider.ini`;
//! a Flatpak exports the same file (`--export-file`, picked up by the host's
//! `exports/share`); for any other install the app writes it to
//! `~/.local/share/gnome-shell/search-providers/` itself at startup.

use std::path::PathBuf;

use gtk::gio;
use gtk::glib::{self, Variant};
use gtk::prelude::*;

use crate::application::DesklinkApplication;
use crate::device_links::daemon::DaemonCommand;

pub const PATH: &str = "/derx06/desklink/com/SearchProvider";
const INTERFACE: &str = "org.gnome.Shell.SearchProvider2";
const INI_NAME: &str = "derx06.desklink.com.search-provider.ini";
/// The provider description, as installed.
pub(crate) const INI: &str = "[Shell Search Provider]
DesktopId=derx06.desklink.com.desktop
BusName=derx06.desklink.com
ObjectPath=/derx06/desklink/com/SearchProvider
Version=2
";

const XML: &str = r#"<node>
  <interface name="org.gnome.Shell.SearchProvider2">
    <method name="GetInitialResultSet">
      <arg type="as" name="terms" direction="in"/>
      <arg type="as" name="results" direction="out"/>
    </method>
    <method name="GetSubsearchResultSet">
      <arg type="as" name="previous_results" direction="in"/>
      <arg type="as" name="terms" direction="in"/>
      <arg type="as" name="results" direction="out"/>
    </method>
    <method name="GetResultMetas">
      <arg type="as" name="identifiers" direction="in"/>
      <arg type="aa{sv}" name="metas" direction="out"/>
    </method>
    <method name="ActivateResult">
      <arg type="s" name="identifier" direction="in"/>
      <arg type="as" name="terms" direction="in"/>
      <arg type="u" name="timestamp" direction="in"/>
    </method>
    <method name="LaunchSearch">
      <arg type="as" name="terms" direction="in"/>
      <arg type="u" name="timestamp" direction="in"/>
    </method>
  </interface>
</node>"#;

/// Something search can find.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// `terminal:<id>`, `command:<id>`, `transfer:<path>`, `ring:<device>`,
    /// `dial:<device>:<number>`, `contact:<device>:<uid>`.
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) icon: &'static str,
    /// What a search term is matched against, lowercased.
    haystack: String,
}

impl Candidate {
    fn new(id: String, name: String, description: String, icon: &'static str, words: &str) -> Self {
        let haystack = format!("{name} {description} {words}").to_lowercase();
        Self {
            id,
            name,
            description,
            icon,
            haystack,
        }
    }
}

/// What there is to find now.
pub(crate) struct Sources<'a> {
    pub(crate) terminals: &'a [crate::device_links::pty::registry::SessionInfo],
    pub(crate) commands: &'a [crate::device_links::config::RunCommand],
    pub(crate) transfers: &'a [crate::device_links::transfer_log::LoggedFile],
    /// Connected phones: id and name.
    pub(crate) phones: &'a [(String, String)],
    /// The contacts paired phones share: phone id, phone name, contact.
    pub(crate) contacts: &'a [(String, String, crate::device_links::contacts::Contact)],
}

/// Every result, in the order shown.
pub(crate) fn candidates(sources: &Sources) -> Vec<Candidate> {
    let mut found = Vec::new();
    for phone in sources.phones {
        found.push(Candidate::new(
            format!("ring:{}", phone.0),
            format!("Ring {}", phone.1),
            "Find your phone".to_string(),
            "audio-volume-high-symbolic",
            "ring find phone",
        ));
    }
    for session in sources
        .terminals
        .iter()
        .filter(|session| session.exit_status.is_none())
    {
        found.push(Candidate::new(
            format!("terminal:{}", session.id),
            crate::terminal_previews::display_title(&session.title),
            match &session.cwd {
                Some(folder) => format!("Shared terminal · {folder}"),
                None => "Shared terminal".to_string(),
            },
            "utilities-terminal-symbolic",
            "terminal shell",
        ));
    }
    for command in sources.commands {
        found.push(Candidate::new(
            format!("command:{}", command.id),
            command.name.clone(),
            format!("Run command · {}", command.command),
            "system-run-symbolic",
            "run command",
        ));
    }
    for file in sources.transfers.iter().take(10) {
        found.push(Candidate::new(
            format!("transfer:{}", file.path.to_string_lossy()),
            file.name.clone(),
            if file.incoming {
                format!("Received from {}", file.device_name)
            } else {
                format!("Sent to {}", file.device_name)
            },
            if file.incoming {
                "folder-download-symbolic"
            } else {
                "document-send-symbolic"
            },
            "file transfer",
        ));
    }
    for (device, phone, contact) in sources.contacts {
        let detail = contact
            .phones
            .first()
            .map(|number| number.number.clone())
            .or_else(|| contact.emails.first().cloned());
        let mut words: Vec<String> = contact
            .phones
            .iter()
            .flat_map(|number| {
                [
                    number.number.clone(),
                    number.number.chars().filter(char::is_ascii_digit).collect(),
                ]
            })
            .collect();
        words.extend(contact.emails.iter().cloned());
        words.extend(contact.org.iter().cloned());
        found.push(Candidate::new(
            format!("contact:{device}:{}", contact.uid),
            contact.name.clone(),
            match detail {
                Some(detail) => format!("{detail} · Contact on {phone}"),
                None => format!("Contact on {phone}"),
            },
            "avatar-default-symbolic",
            &words.join(" "),
        ));
    }
    found
}

/// A typed phone number: "Call <number>" with the first phone that can
/// (`callers`: connected phones taking call requests, id and name).
pub(crate) fn dial_candidate(callers: &[(String, String)], terms: &[String]) -> Option<Candidate> {
    let number = crate::device_links::calls::dial_number(&terms.join(" "))?;
    let (id, name) = callers.first()?;
    Some(Candidate::new(
        format!("dial:{id}:{number}"),
        format!("Call {number}"),
        format!("With {name}"),
        "call-start-symbolic",
        "",
    ))
}

/// The "Call" result a `dial:` id stands for, while that phone can still
/// call.
fn dial_from_id(callers: &[(String, String)], id: &str) -> Option<Candidate> {
    let (device, number) = id.strip_prefix("dial:")?.split_once(':')?;
    let caller = callers.iter().find(|(known, _)| known == device)?;
    dial_candidate(std::slice::from_ref(caller), &[number.to_string()])
        .filter(|candidate| candidate.id == id)
}

/// Connected phones that take call requests: id and name.
fn callers(app: &DesklinkApplication) -> Vec<(String, String)> {
    app.daemon_handle()
        .map(|daemon| {
            daemon
                .devices()
                .into_iter()
                .filter(|device| device.trusted && device.feature_transport_ready)
                .filter(|device| {
                    device.incoming_capabilities.iter().any(|capability| {
                        capability == crate::device_links::packet::PACKET_TYPE_TELEPHONY_REQUEST
                    })
                })
                .map(|device| (device.id, device.name))
                .collect()
        })
        .unwrap_or_default()
}

/// What search shows for `terms`: every result there is, and the ids found
/// (a number to call first).
pub(crate) fn search(app: &DesklinkApplication, terms: &[String]) -> (Vec<Candidate>, Vec<String>) {
    let mut all = current(app);
    let mut found = matching(&all, terms);
    if let Some(dial) = dial_candidate(&callers(app), terms) {
        found.insert(0, dial.id.clone());
        all.push(dial);
    }
    (all, found)
}

/// The ids of `candidates` that contain every term (case-insensitive).
pub(crate) fn matching(candidates: &[Candidate], terms: &[String]) -> Vec<String> {
    let terms: Vec<String> = terms
        .iter()
        .map(|term| term.trim().to_lowercase())
        .filter(|term| !term.is_empty())
        .collect();
    if terms.is_empty() {
        return Vec::new();
    }
    candidates
        .iter()
        .filter(|candidate| terms.iter().all(|term| candidate.haystack.contains(term)))
        .map(|candidate| candidate.id.clone())
        .collect()
}

pub(crate) fn current(app: &DesklinkApplication) -> Vec<Candidate> {
    let terminals = crate::terminal_previews::sessions();
    let commands = crate::device_links::config::app_settings().run_commands;
    let transfers = crate::device_links::transfer_log::recent();
    let phones: Vec<(String, String)> = app
        .daemon_handle()
        .map(|daemon| {
            daemon
                .devices()
                .into_iter()
                .filter(|device| device.trusted && device.feature_transport_ready)
                .map(|device| (device.id, device.name))
                .collect()
        })
        .unwrap_or_default();
    // Paired phones' contacts, connected or not: the cache is local.
    let contacts: Vec<_> = app
        .daemon_handle()
        .map(|daemon| daemon.devices())
        .unwrap_or_default()
        .into_iter()
        .filter(|device| device.trusted)
        .flat_map(|device| {
            crate::device_links::contacts::list(&device.id)
                .into_iter()
                .map(move |contact| (device.id.clone(), device.name.clone(), contact))
        })
        .collect();
    candidates(&Sources {
        terminals: &terminals,
        commands: &commands,
        transfers: &transfers,
        phones: &phones,
        contacts: &contacts,
    })
}

fn metas(candidates: &[Candidate], ids: &[String]) -> Variant {
    let dicts = ids.iter().filter_map(|id| {
        let candidate = candidates.iter().find(|candidate| &candidate.id == id)?;
        let dict = glib::VariantDict::new(None);
        dict.insert("id", &candidate.id);
        dict.insert("name", &candidate.name);
        dict.insert("description", &candidate.description);
        let icon = IconExt::to_string(&gio::ThemedIcon::new(candidate.icon))
            .map(|icon| icon.to_string())
            .unwrap_or_else(|| candidate.icon.to_string());
        dict.insert("gicon", icon.as_str());
        Some(dict.end())
    });
    Variant::array_from_iter_with_type(glib::VariantTy::VARDICT, dicts)
}

/// Does what result `id` stands for.
pub(crate) fn activate(app: &DesklinkApplication, id: &str) {
    let Some((kind, rest)) = id.split_once(':') else {
        return;
    };
    // The user typed the number and chose Call: no second question.
    if kind == "dial" {
        if let (Some(_), Some((device, number)), Some(daemon)) = (
            dial_from_id(&callers(app), id),
            rest.split_once(':'),
            app.daemon_handle(),
        ) {
            daemon.send(DaemonCommand::Telephony(
                device.to_string(),
                "dial".to_string(),
                Some(number.to_string()),
            ));
        }
        return;
    }
    // Only what search itself offered: the id is looked up again.
    if !current(app).iter().any(|candidate| candidate.id == id) {
        return;
    }
    match kind {
        "terminal" => crate::device_links::pty::desk::open_on_desktop(rest),
        "command" => {
            let commands = crate::device_links::config::app_settings().run_commands;
            if let Some(command) = commands.iter().find(|command| command.id == rest) {
                match crate::device_links::pty::registry::run_command(
                    &command.name,
                    &command.command,
                    "desk",
                ) {
                    Ok(info) => crate::device_links::pty::desk::open_on_desktop(&info.id),
                    Err(error) => eprintln!("[Search] Could not run {}: {error}", command.name),
                }
            }
        }
        "transfer" => {
            let uri = gio::File::for_path(rest).uri();
            if let Err(error) =
                gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE)
            {
                eprintln!("[Search] Could not open {rest}: {error}");
            }
        }
        "contact" => {
            if let Some((device, uid)) = rest.split_once(':') {
                let window = app.main_window();
                window.present();
                window.show_contact(device, uid);
            }
        }
        "ring" => {
            if let Some(daemon) = app.daemon_handle() {
                daemon.send(DaemonCommand::SendFindPhone(rest.to_string()));
            }
        }
        _ => {}
    }
}

/// Exports the provider on the application's connection.
pub fn export(app: &DesklinkApplication, connection: &gio::DBusConnection) {
    let Some(interface) = gio::DBusNodeInfo::for_xml(XML)
        .ok()
        .and_then(|node| node.lookup_interface(INTERFACE))
    else {
        return;
    };
    let weak = app.downgrade();
    let registered = connection
        .register_object(PATH, &interface)
        .method_call(move |_, _, _, _, method, params, invocation| {
            let Some(app) = weak.upgrade() else {
                return invocation.return_value(None);
            };
            match method {
                "GetInitialResultSet" => {
                    let terms = params.get::<(Vec<String>,)>().unwrap_or_default().0;
                    let (_, found) = search(&app, &terms);
                    invocation.return_value(Some(&(found,).to_variant()));
                }
                "GetSubsearchResultSet" => {
                    let (previous, terms) = params
                        .get::<(Vec<String>, Vec<String>)>()
                        .unwrap_or_default();
                    // A number typed further is a new "Call" result.
                    let found: Vec<String> = search(&app, &terms)
                        .1
                        .into_iter()
                        .filter(|id| previous.contains(id) || id.starts_with("dial:"))
                        .collect();
                    invocation.return_value(Some(&(found,).to_variant()));
                }
                "GetResultMetas" => {
                    let ids = params.get::<(Vec<String>,)>().unwrap_or_default().0;
                    let mut all = current(&app);
                    let callers = callers(&app);
                    all.extend(ids.iter().filter_map(|id| dial_from_id(&callers, id)));
                    let metas = metas(&all, &ids);
                    invocation.return_value(Some(&Variant::tuple_from_iter([metas])));
                }
                "ActivateResult" => {
                    if let Some(id) = params.try_child_value(0).and_then(|id| id.get::<String>()) {
                        activate(&app, &id);
                    }
                    invocation.return_value(None);
                }
                _ => {
                    app.activate();
                    invocation.return_value(None);
                }
            }
        })
        .build();
    if let Err(error) = registered {
        eprintln!("[Search] Could not export {PATH}: {error}");
    }
}

/// Where the provider description goes for this user, unless a package or
/// a Flatpak export already provides one.
fn user_ini() -> Option<PathBuf> {
    let installed = glib::system_data_dirs().iter().any(|dir| {
        dir.join("gnome-shell/search-providers")
            .join(INI_NAME)
            .exists()
    });
    (!installed).then(|| {
        glib::user_data_dir()
            .join("gnome-shell/search-providers")
            .join(INI_NAME)
    })
}

/// Writes the provider description for this user when it is missing or
/// different (GNOME reads it at its next start).
pub fn install_description() {
    // Only GNOME Shell reads these.
    if !crate::shell_dbus::is_gnome(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()) {
        return;
    }
    let Some(path) = user_ini() else {
        return;
    };
    if std::fs::read_to_string(&path).ok().as_deref() == Some(INI) {
        return;
    }
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, INI));
    if let Err(error) = written {
        eprintln!("[Search] Could not write {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::config::RunCommand;
    use crate::device_links::pty::registry::SessionInfo;
    use crate::device_links::transfer_log::LoggedFile;

    fn sources_and<R>(run: impl FnOnce(&Sources) -> R) -> R {
        let terminals = [
            SessionInfo {
                id: "t1".into(),
                title: "-bash".into(),
                rows: 24,
                cols: 80,
                exit_status: None,
                desk_window: false,
                cwd: Some("~/api".into()),
                started_at: 0,
                shell: Some("bash".into()),
            },
            SessionInfo {
                id: "t2".into(),
                title: "htop".into(),
                rows: 24,
                cols: 80,
                exit_status: Some(0),
                desk_window: false,
                cwd: None,
                started_at: 0,
                shell: None,
            },
        ];
        let commands = [RunCommand {
            id: "c1".into(),
            name: "Deploy staging".into(),
            command: "./deploy.sh".into(),
        }];
        let transfers = [LoggedFile {
            ts: 1,
            device_id: "p1".into(),
            device_name: "Pixel".into(),
            name: "boarding-pass.pdf".into(),
            path: "/home/me/Downloads/boarding-pass.pdf".into(),
            size: 10,
            incoming: true,
        }];
        let phones = [("p1".to_string(), "Pixel".to_string())];
        let contacts = [(
            "p1".to_string(),
            "Pixel".to_string(),
            crate::device_links::contacts::parse_vcard(
                "u:1",
                "FN:Mara Lindqvist\nTEL:+46 70-123 45 67\nEMAIL:mara@example.org",
            )
            .unwrap(),
        )];
        run(&Sources {
            terminals: &terminals,
            commands: &commands,
            transfers: &transfers,
            phones: &phones,
            contacts: &contacts,
        })
    }

    #[test]
    fn every_kind_of_result_is_found_by_its_words() {
        sources_and(|sources| {
            let all = candidates(sources);
            let ids = |terms: &[&str]| {
                matching(
                    &all,
                    &terms
                        .iter()
                        .map(|term| term.to_string())
                        .collect::<Vec<_>>(),
                )
            };
            assert_eq!(ids(&["ring"]), ["ring:p1"]);
            assert_eq!(
                ids(&["PIXEL", "find"]),
                ["ring:p1"],
                "case-insensitive, all terms"
            );
            assert_eq!(
                ids(&["api"]),
                ["terminal:t1"],
                "a running terminal, by folder"
            );
            assert!(ids(&["htop"]).is_empty(), "an ended one is not offered");
            assert_eq!(ids(&["deploy"]), ["command:c1"]);
            assert_eq!(
                ids(&["boarding"]),
                ["transfer:/home/me/Downloads/boarding-pass.pdf"]
            );
            // A shared contact, by name, number (as typed or digits) or email.
            for terms in [&["mara"][..], &["lindq"], &["701234567"], &["example.org"]] {
                assert_eq!(ids(terms), ["contact:p1:u:1"], "{terms:?}");
            }
            assert!(ids(&[]).is_empty() && ids(&["  "]).is_empty());
            assert!(ids(&["nothing-like-this"]).is_empty());
        });
    }

    #[test]
    fn a_typed_number_offers_a_call_with_the_phone() {
        let callers = [("p1".to_string(), "Pixel".to_string())];
        let terms =
            |text: &str| -> Vec<String> { text.split_whitespace().map(str::to_string).collect() };
        let call = dial_candidate(&callers, &terms("+1 555 0100")).unwrap();
        assert_eq!(call.id, "dial:p1:+15550100");
        assert_eq!(call.name, "Call +15550100");
        assert_eq!(call.description, "With Pixel");
        assert_eq!(dial_from_id(&callers, &call.id), Some(call));
        assert!(dial_candidate(&callers, &terms("deploy")).is_none());
        assert!(
            dial_candidate(&[], &terms("5550100")).is_none(),
            "no phone can call"
        );
        assert!(
            dial_from_id(&callers, "dial:p2:5550100").is_none(),
            "another phone"
        );
        assert!(
            dial_from_id(&callers, "dial:p1:555-0100").is_none(),
            "not as offered"
        );
    }

    #[test]
    fn result_metas_carry_name_description_and_icon() {
        sources_and(|sources| {
            let all = candidates(sources);
            let metas = metas(&all, &["command:c1".to_string(), "gone".to_string()]);
            assert_eq!(metas.n_children(), 1, "unknown ids are skipped");
            let meta = glib::VariantDict::new(Some(&metas.child_value(0)));
            assert_eq!(
                meta.lookup::<String>("name").unwrap().unwrap(),
                "Deploy staging"
            );
            assert_eq!(
                meta.lookup::<String>("description").unwrap().unwrap(),
                "Run command · ./deploy.sh"
            );
            assert!(meta
                .lookup::<String>("gicon")
                .unwrap()
                .unwrap()
                .contains("system-run"));
        });
    }

    #[test]
    fn the_description_names_this_app() {
        assert!(INI.contains("BusName=derx06.desklink.com"));
        assert!(INI.contains(&format!("ObjectPath={PATH}")));
        assert!(INI.contains("Version=2"));
    }
}
