//! How this copy of DeskLink was installed, and so how to update it
//! (`docs/UPDATES.md` 5.1). Pure functions over an injected [`Env`]; the one
//! reader of the real machine is [`Env::detect`]. The text it produces is
//! derived here, never taken from a manifest, and **nothing is ever run**: the
//! commands are shown to be copied.
//!
//! Sibling edges: none. Outside: std only.

use std::path::{Path, PathBuf};

/// What detection needs to know about the machine.
#[derive(Debug, Clone, Default)]
pub(crate) struct Env {
    /// `/.flatpak-info` exists.
    pub(crate) flatpak_info: bool,
    /// `$APPIMAGE`, when set and the file exists.
    pub(crate) appimage: Option<PathBuf>,
    pub(crate) exe: PathBuf,
    /// The text of `/etc/os-release` ("" when unreadable).
    pub(crate) os_release: String,
    pub(crate) home: Option<PathBuf>,
}

impl Env {
    pub(crate) fn detect() -> Env {
        Env {
            flatpak_info: Path::new("/.flatpak-info").exists(),
            appimage: std::env::var_os("APPIMAGE")
                .map(PathBuf::from)
                .filter(|path| path.exists()),
            exe: std::env::current_exe().unwrap_or_default(),
            os_release: std::fs::read_to_string("/etc/os-release").unwrap_or_default(),
            home: dirs::home_dir(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Arch,
    Debian,
    Fedora,
    Suse,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Flatpak,
    AppImage,
    Nix,
    Package(Family),
    /// `~/.local/bin`, cargo, anything under the home folder or elsewhere.
    Manual,
}

/// What the person is told to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Guidance {
    /// A command to copy, with a note under it when there is one.
    Command {
        command: &'static str,
        note: Option<&'static str>,
    },
    /// A sentence, no command (a package manager we do not name).
    Text(&'static str),
    /// Open the release page.
    ReleasePage(&'static str),
}

pub(crate) fn detect(env: &Env) -> Source {
    let exe = env.exe.as_path();
    if env.flatpak_info {
        Source::Flatpak
    } else if env.appimage.is_some() {
        Source::AppImage
    } else if exe.starts_with("/nix/store") {
        Source::Nix
    } else if exe.starts_with("/usr") && !under_home(exe, env) {
        Source::Package(family(&env.os_release))
    } else {
        Source::Manual
    }
}

fn under_home(exe: &Path, env: &Env) -> bool {
    env.home
        .as_deref()
        .is_some_and(|home| exe.starts_with(home))
}

/// The package family from `ID` and the words of `ID_LIKE`.
pub(crate) fn family(os_release: &str) -> Family {
    let mut words: Vec<String> = Vec::new();
    for line in os_release.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key == "ID" || key == "ID_LIKE" {
            words.extend(
                value
                    .trim()
                    .trim_matches(['"', '\''])
                    .split_whitespace()
                    .map(str::to_ascii_lowercase),
            );
        }
    }
    let has = |names: &[&str]| words.iter().any(|w| names.contains(&w.as_str()));
    if has(&["arch", "manjaro", "endeavouros", "cachyos"]) {
        Family::Arch
    } else if has(&["debian", "ubuntu", "linuxmint", "pop"]) {
        Family::Debian
    } else if has(&["fedora", "rhel", "centos"]) {
        Family::Fedora
    } else if words
        .iter()
        .any(|w| w.starts_with("opensuse") || w == "suse")
    {
        Family::Suse
    } else {
        Family::Other
    }
}

impl Source {
    /// "Installed with ..." (section 6.1).
    pub(crate) fn label(self) -> &'static str {
        match self {
            Source::Flatpak => "Flatpak",
            Source::AppImage => "AppImage",
            Source::Nix => "Nix",
            Source::Package(Family::Arch) => "Arch (AUR)",
            Source::Package(Family::Debian) => "Debian or Ubuntu package",
            Source::Package(Family::Fedora) => "Fedora package",
            Source::Package(Family::Suse) => "openSUSE package",
            Source::Package(Family::Other) => "a package",
            Source::Manual => "a download",
        }
    }

    pub(crate) fn guidance(self) -> Guidance {
        match self {
            Source::Flatpak => Guidance::Command {
                command: "flatpak update derx06.desklink.com",
                note: None,
            },
            Source::AppImage => Guidance::ReleasePage(
                "Download the new AppImage from the release page and replace this file. DeskLink can't update itself.",
            ),
            Source::Nix => Guidance::Text("Update through your Nix configuration."),
            Source::Package(Family::Arch) => Guidance::Command {
                command: "yay -Syu desklink",
                note: Some("or your AUR helper. Use desklink-bin if that is the one you installed."),
            },
            Source::Package(Family::Debian) => Guidance::Command {
                command: "sudo apt update && sudo apt install --only-upgrade desklink",
                note: None,
            },
            Source::Package(Family::Fedora) => Guidance::Command {
                command: "sudo dnf upgrade desklink",
                note: None,
            },
            Source::Package(Family::Suse) => Guidance::Command {
                command: "sudo zypper refresh && sudo zypper update desklink",
                note: None,
            },
            Source::Package(Family::Other) => {
                Guidance::Text("Update DeskLink with your package manager.")
            }
            Source::Manual => Guidance::ReleasePage("Download the new version from the release page."),
        }
    }

    /// Whether a distribution may lag behind the release ("Your distribution
    /// may not have {version} yet.").
    pub(crate) fn may_lag(self) -> bool {
        matches!(self, Source::Flatpak | Source::Package(_))
    }
}
