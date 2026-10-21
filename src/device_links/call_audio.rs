//! Experimental: the phone's calls through this computer's speaker and
//! microphone, over Bluetooth (contract T5; local, nothing on the wire).
//! BlueZ, over D-Bus, connects the phone's hands-free profile; PipeWire's
//! built-in hands-free role carries the audio. Nothing to install: the only
//! step is pairing the phone over Bluetooth once, in the desktop's own
//! Bluetooth settings.

use gtk::gio;
use gtk::glib::{self, prelude::*};

/// The phone side of hands-free (HFP Audio Gateway).
pub(crate) const HFP_AG_UUID: &str = "0000111f-0000-1000-8000-00805f9b34fb";

/// A Bluetooth device bonded with this computer that can carry calls.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct BondedPhone {
    pub(crate) path: String,
    pub(crate) address: String,
    pub(crate) name: String,
    /// Its Bluetooth link is up (calls, music or both).
    pub(crate) connected: bool,
}

fn system_bus() -> Option<gio::DBusConnection> {
    gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE).ok()
}

/// One BlueZ `Device1`, if it is paired and offers calls.
fn bonded_phone(path: &str, device: &glib::VariantDict) -> Option<BondedPhone> {
    let flag = |name: &str| device.lookup::<bool>(name).ok().flatten().unwrap_or(false);
    let uuids: Vec<String> = device.lookup("UUIDs").ok().flatten().unwrap_or_default();
    if !(flag("Paired") || flag("Bonded"))
        || !uuids
            .iter()
            .any(|uuid| uuid.eq_ignore_ascii_case(HFP_AG_UUID))
    {
        return None;
    }
    let address: String = device.lookup("Address").ok().flatten()?;
    let name: String = device
        .lookup("Alias")
        .ok()
        .flatten()
        .or_else(|| device.lookup("Name").ok().flatten())
        .unwrap_or_else(|| address.clone());
    Some(BondedPhone {
        path: path.to_string(),
        address,
        name,
        connected: flag("Connected"),
    })
}

/// The bonded devices that can carry calls; `None` when Bluetooth is not
/// running here (BlueZ is not started for this).
pub(crate) fn bonded_phones() -> Option<Vec<BondedPhone>> {
    let reply = system_bus()?
        .call_sync(
            Some("org.bluez"),
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "GetManagedObjects",
            None,
            Some(glib::VariantTy::new("(a{oa{sa{sv}}})").expect("valid type")),
            gio::DBusCallFlags::NO_AUTO_START,
            1000,
            gio::Cancellable::NONE,
        )
        .ok()?;
    let objects = reply.child_value(0);
    let mut phones = Vec::new();
    for index in 0..objects.n_children() {
        let entry = objects.child_value(index);
        let Some(path) = entry.child_value(0).str().map(str::to_string) else {
            continue;
        };
        let interfaces = glib::VariantDict::new(Some(&entry.child_value(1)));
        if let Some(device) = interfaces.lookup_value("org.bluez.Device1", None) {
            phones.extend(bonded_phone(&path, &glib::VariantDict::new(Some(&device))));
        }
    }
    phones.sort_by(|a, b| a.name.cmp(&b.name));
    Some(phones)
}

fn words(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The bonded device most likely to be the phone named `phone_name`: the
/// same name (ignoring case, spaces and punctuation), else one name inside
/// the other, else the only one there is. The user still confirms it once.
pub(crate) fn likely_match<'a>(
    phones: &'a [BondedPhone],
    phone_name: &str,
) -> Option<&'a BondedPhone> {
    let wanted = words(phone_name);
    phones
        .iter()
        .find(|phone| words(&phone.name) == wanted)
        .or_else(|| {
            phones.iter().find(|phone| {
                let name = words(&phone.name);
                !name.is_empty() && (name.contains(&wanted) || wanted.contains(&name))
            })
        })
        .or_else(|| (phones.len() == 1).then(|| &phones[0]))
}

/// Connects (`true`) or disconnects the phone's hands-free profile: calls
/// then use this computer, or go back to the phone. `done` runs on the
/// calling thread's main context, with BlueZ's error in its words.
pub(crate) fn use_for_calls(
    path: &str,
    connect: bool,
    done: impl FnOnce(Result<(), String>) + 'static,
) {
    let Some(bus) = system_bus() else {
        done(Err("Bluetooth is not available".to_string()));
        return;
    };
    bus.call(
        Some("org.bluez"),
        path,
        "org.bluez.Device1",
        if connect {
            "ConnectProfile"
        } else {
            "DisconnectProfile"
        },
        Some(&(HFP_AG_UUID,).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        30_000,
        gio::Cancellable::NONE,
        move |result| {
            done(result.map(|_| ()).map_err(|error| {
                // "GDBus.Error:org.bluez.Error.Failed: <what>" → "<what>".
                let message = error.message();
                match message.split_once(": ") {
                    Some((name, what)) if name.starts_with("GDBus.Error:") => what.to_string(),
                    _ => message.to_string(),
                }
            }))
        },
    );
}

/// Opens this desktop's Bluetooth settings, whichever it has: GNOME's,
/// KDE's or Blueman. `false` when none is installed.
pub(crate) fn open_bluetooth_settings() -> bool {
    [
        &["gnome-control-center", "bluetooth"][..],
        &["systemsettings", "kcm_bluetooth"],
        &["blueman-manager"],
    ]
    .iter()
    .filter(|argv| glib::find_program_in_path(argv[0]).is_some())
    .any(|argv| {
        let argv: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
        gio::Subprocess::newv(&argv, gio::SubprocessFlags::NONE).is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(paired: bool, uuids: &[&str], alias: &str) -> glib::VariantDict {
        let dict = glib::VariantDict::new(None);
        dict.insert("Paired", paired);
        dict.insert("Connected", true);
        dict.insert("Address", "AA:BB:CC:DD:EE:FF");
        dict.insert("Alias", alias);
        dict.insert(
            "UUIDs",
            uuids
                .iter()
                .map(|uuid| uuid.to_string())
                .collect::<Vec<_>>(),
        );
        dict
    }

    #[test]
    fn only_a_paired_device_offering_calls_counts() {
        let phone = bonded_phone(
            "/org/bluez/hci0/dev_1",
            &device(true, &[HFP_AG_UUID], "Pixel 9"),
        );
        assert_eq!(
            phone,
            Some(BondedPhone {
                path: "/org/bluez/hci0/dev_1".into(),
                address: "AA:BB:CC:DD:EE:FF".into(),
                name: "Pixel 9".into(),
                connected: true,
            })
        );
        assert!(bonded_phone("/d", &device(false, &[HFP_AG_UUID], "Pixel")).is_none());
        let headphones = "0000111e-0000-1000-8000-00805f9b34fb";
        assert!(bonded_phone("/d", &device(true, &[headphones], "Buds")).is_none());
    }

    #[test]
    fn the_phone_is_found_by_its_name() {
        let phone = |name: &str| BondedPhone {
            path: format!("/{name}"),
            address: String::new(),
            name: name.into(),
            connected: false,
        };
        let both = [phone("Galaxy Buds"), phone("realme 14 5G")];
        assert_eq!(
            likely_match(&both, "Realme 14 5g").map(|p| p.name.as_str()),
            Some("realme 14 5G")
        );
        assert_eq!(
            likely_match(&both, "realme").map(|p| p.name.as_str()),
            Some("realme 14 5G"),
            "one name inside the other"
        );
        assert_eq!(
            likely_match(&both, "Pixel"),
            None,
            "two others: ask nothing"
        );
        assert_eq!(
            likely_match(&both[1..], "Pixel").map(|p| p.name.as_str()),
            Some("realme 14 5G"),
            "the only one there is"
        );
    }
}
