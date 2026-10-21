//! KDE Plasma extras, used only when Plasma is the desktop: a KRunner runner
//! (`org.kde.krunner1` on the app's own D-Bus name) that finds what GNOME's
//! search provider finds — terminals, run commands, recent files, phones —
//! and the file that tells KRunner about it. Never required: everything is
//! also in the app and its tray menu.

use gtk::gio;
use gtk::glib::{self, Variant};
use gtk::prelude::*;

use crate::application::DesklinkApplication;

pub const PATH: &str = "/derx06/desklink/com/KRunner";
const INTERFACE: &str = "org.kde.krunner1";
/// `KRunner::QueryMatch::PossibleMatch`.
const POSSIBLE_MATCH: i32 = 30;

const XML: &str = r#"<node>
  <interface name="org.kde.krunner1">
    <method name="Actions">
      <arg name="matches" type="a(sss)" direction="out"/>
    </method>
    <method name="Match">
      <arg name="query" type="s" direction="in"/>
      <arg name="matches" type="a(sssida{sv})" direction="out"/>
    </method>
    <method name="Run">
      <arg name="matchId" type="s" direction="in"/>
      <arg name="actionId" type="s" direction="in"/>
    </method>
    <method name="Teardown"/>
  </interface>
</node>"#;

/// The runner's description, in `krunner/dbusplugins/`.
pub(crate) const RUNNER: &str = "[Desktop Entry]
Name=DeskLink
Comment=Shared terminals, run commands, recent files and your phone
Icon=derx06.desklink.com
Type=Service
X-KDE-ServiceTypes=Plasma/Runner
X-KDE-PluginInfo-Name=derx06.desklink.com
X-KDE-PluginInfo-EnabledByDefault=true
X-Plasma-API=DBus
X-Plasma-API-Minimum-Version=2.0
X-Plasma-DBusRunner-Service=derx06.desklink.com
X-Plasma-DBusRunner-Path=/derx06/desklink/com/KRunner
";

/// Whether Plasma is the desktop (`XDG_CURRENT_DESKTOP` lists KDE).
pub(crate) fn is_kde(current_desktop: &str) -> bool {
    current_desktop
        .split(':')
        .any(|desktop| desktop.eq_ignore_ascii_case("KDE"))
}

fn on_kde() -> bool {
    is_kde(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
}

/// KRunner's matches for `query`, from the same results as GNOME's search.
pub(crate) fn matches(candidates: &[crate::search_provider::Candidate], query: &str) -> Variant {
    let terms: Vec<String> = query.split_whitespace().map(str::to_string).collect();
    let found = crate::search_provider::matching(candidates, &terms);
    let rows = candidates
        .iter()
        .filter(|candidate| found.contains(&candidate.id))
        .map(|candidate| {
            let properties = glib::VariantDict::new(None);
            properties.insert("subtext", &candidate.description);
            Variant::tuple_from_iter([
                candidate.id.to_variant(),
                candidate.name.to_variant(),
                candidate.icon.to_variant(),
                POSSIBLE_MATCH.to_variant(),
                0.7f64.to_variant(),
                properties.end(),
            ])
        });
    Variant::array_from_iter_with_type(
        glib::VariantTy::new("(sssida{sv})").expect("valid type"),
        rows,
    )
}

/// Exports the runner on the application's connection (only on Plasma).
pub fn export(app: &DesklinkApplication, connection: &gio::DBusConnection) {
    if !on_kde() {
        return;
    }
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
                "Actions" => invocation.return_value(Some(
                    &(Vec::<(String, String, String)>::new(),).to_variant(),
                )),
                "Match" => {
                    let query = params.get::<(String,)>().unwrap_or_default().0;
                    let found = matches(&crate::search_provider::current(&app), &query);
                    invocation.return_value(Some(&Variant::tuple_from_iter([found])));
                }
                "Run" => {
                    if let Some(id) = params.try_child_value(0).and_then(|id| id.get::<String>()) {
                        crate::search_provider::activate(&app, &id);
                    }
                    invocation.return_value(None);
                }
                _ => invocation.return_value(None),
            }
        })
        .build();
    if let Err(error) = registered {
        eprintln!("[KDE] Could not export {PATH}: {error}");
    }
}

/// Writes the runner description for this user on Plasma (KRunner reads it
/// at its next start).
pub fn install_runner() {
    if !on_kde() {
        return;
    }
    let path = glib::user_data_dir()
        .join("krunner/dbusplugins")
        .join("derx06.desklink.com.desktop");
    if std::fs::read_to_string(&path).ok().as_deref() == Some(RUNNER) {
        return;
    }
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, RUNNER));
    if let Err(error) = written {
        eprintln!("[KDE] Could not write {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search_provider::{candidates, Sources};

    #[test]
    fn plasma_is_told_apart() {
        assert!(is_kde("KDE"));
        assert!(is_kde("ubuntu:KDE"));
        assert!(!is_kde("GNOME"));
        assert!(!is_kde(""));
    }

    #[test]
    fn krunner_gets_the_same_results_as_gnome_search() {
        let phones = [("p1".to_string(), "Pixel".to_string())];
        let all = candidates(&Sources {
            terminals: &[],
            commands: &[],
            transfers: &[],
            phones: &phones,
        });
        let found = matches(&all, "ring pix");
        assert_eq!(found.type_().as_str(), "a(sssida{sv})");
        assert_eq!(found.n_children(), 1);
        let row = found.child_value(0);
        assert_eq!(row.child_value(0).str(), Some("ring:p1"));
        assert_eq!(row.child_value(1).str(), Some("Ring Pixel"));
        assert_eq!(row.child_value(3).get::<i32>(), Some(POSSIBLE_MATCH));
        assert_eq!(matches(&all, "nothing").n_children(), 0);
    }

    #[test]
    fn the_runner_points_at_this_app() {
        assert!(RUNNER.contains("X-Plasma-DBusRunner-Service=derx06.desklink.com\n"));
        assert!(RUNNER.contains(&format!("X-Plasma-DBusRunner-Path={PATH}\n")));
        assert!(RUNNER.contains("X-Plasma-API=DBus"));
    }
}
