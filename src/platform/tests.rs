//! Tests of the system check: every desktop family and missing piece through
//! `evaluate` on faked probes, the report text, sticky notices, discovery
//! sharers and the D-Bus error rules.

use super::*;

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|name| name.to_string()).collect()
}

/// A fully equipped machine; each test removes what it is about.
fn full() -> Probe {
    Probe {
        desktop: "GNOME".into(),
        session_type: Some("wayland".into()),
        wayland_display: true,
        x11_display: true,
        runtime_dir: true,
        session_bus: true,
        system_bus: true,
        session_names: names(&[PORTAL, NOTIFICATIONS, WATCHER]),
        system_names: names(&[LOGIND]),
        tray_host: true,
        portals: names(&["ScreenCast", "RemoteDesktop", "InputCapture", "Background"]),
        screencast_sources: Some(7),
        input_capture: Some(3),
        elements: WATCHED_ELEMENTS.iter().map(|e| e.to_string()).collect(),
        pipewire_socket: true,
        terminals: names(&["xterm"]),
        config_writable: true,
        toolkit: "GTK 4.14, libadwaita 1.5.".into(),
        ..Probe::default()
    }
}

fn level(rows: &[Check], id: &str) -> Level {
    rows.iter().find(|row| row.id == id).unwrap().level
}

fn row<'a>(rows: &'a [Check], id: &str) -> &'a Check {
    rows.iter().find(|row| row.id == id).unwrap()
}

#[test]
fn a_fully_equipped_gnome_wayland_machine_has_nothing_missing() {
    let rows = evaluate(&full());
    for row in &rows {
        assert_eq!(row.level, Level::Ok, "{}: {}", row.title, row.detail);
    }
}

#[test]
fn desktop_names_from_every_desktop_are_recognised() {
    for (value, expected) in [
        ("GNOME", Family::Gnome),
        ("ubuntu:GNOME", Family::Gnome),
        ("KDE", Family::Kde),
        ("X-Cinnamon", Family::Classic),
        ("XFCE", Family::Classic),
        ("MATE", Family::Classic),
        ("Budgie:GNOME", Family::Classic),
        ("GNOME-Flashback:GNOME", Family::Classic),
        ("GNOME-Classic:GNOME", Family::Gnome),
        ("pop:GNOME", Family::Gnome),
        ("Unity:Unity7:ubuntu", Family::Classic),
        ("COSMIC", Family::Cosmic),
        ("sway", Family::Wlroots),
        ("Hyprland", Family::Wlroots),
        ("river", Family::Wlroots),
        ("LXQt", Family::Classic),
        ("", Family::BareWm),
        ("i3", Family::BareWm),
        (":", Family::BareWm),
    ] {
        assert_eq!(family(value), expected, "{value:?}");
    }
    assert_eq!(desktop_name("ubuntu:GNOME"), "GNOME");
    assert!(desktop_name("").contains("unnamed"));
}

#[test]
fn a_window_manager_without_autostart_is_told_what_line_to_add() {
    for desktop in ["", "i3", "sway", "Hyprland", "openbox"] {
        let rows = evaluate(&Probe {
            desktop: desktop.into(),
            ..full()
        });
        let row = row(&rows, "autostart");
        assert_eq!(row.level, Level::Limited, "{desktop:?}");
        assert!(row.detail.contains("exec desklink --gapplication-service"));
    }
    for desktop in ["GNOME", "KDE", "XFCE", "X-Cinnamon", "MATE", "COSMIC"] {
        let rows = evaluate(&Probe {
            desktop: desktop.into(),
            ..full()
        });
        assert_eq!(level(&rows, "autostart"), Level::Ok, "{desktop:?}");
    }
}

#[test]
fn no_tray_is_limited_and_says_how_to_reopen_the_window() {
    let rows = evaluate(&Probe {
        tray_host: false,
        ..full()
    });
    let tray = row(&rows, "tray");
    assert_eq!(tray.level, Level::Limited);
    assert!(tray.detail.contains("AppIndicator"));
    assert!(tray.install.is_some());
    let rows = evaluate(&Probe {
        tray_host: false,
        desktop: "sway".into(),
        ..full()
    });
    assert!(row(&rows, "tray")
        .detail
        .contains("running `desklink` again"));
    assert!(row(&rows, "tray").install.is_none());
}

#[test]
fn no_notification_daemon_is_said_plainly() {
    let rows = evaluate(&Probe {
        session_names: names(&[PORTAL]),
        ..full()
    });
    let row = row(&rows, "notifications");
    assert_eq!(row.level, Level::Limited);
    assert!(row.install.unwrap().arch.contains("dunst"));
}

#[test]
fn x11_without_a_portal_shares_through_x11_directly() {
    let probe = Probe {
        desktop: "".into(),
        session_type: None,
        wayland_display: false,
        x11_display: true,
        session_names: names(&[NOTIFICATIONS]),
        portals: BTreeSet::new(),
        screencast_sources: None,
        input_capture: None,
        ..full()
    };
    let rows = evaluate(&probe);
    assert_eq!(level(&rows, "share"), Level::Ok);
    assert!(row(&rows, "share").tech.contains("X11 directly"));
    // And needs the X11 capture element.
    let mut bare = probe.clone();
    bare.elements.remove("ximagesrc");
    assert_eq!(level(&evaluate(&bare), "share"), Level::Missing);
    // Extend and Glide are Wayland-only.
    assert_eq!(level(&rows, "extend"), Level::Missing);
    assert_eq!(level(&rows, "glide"), Level::Missing);
}

#[test]
fn wayland_without_a_portal_names_the_backend_for_the_desktop() {
    for (desktop, backend) in [
        ("GNOME", "xdg-desktop-portal-gnome"),
        ("KDE", "xdg-desktop-portal-kde"),
        ("sway", "xdg-desktop-portal-wlr"),
        ("COSMIC", "xdg-desktop-portal-cosmic"),
    ] {
        let rows = evaluate(&Probe {
            desktop: desktop.into(),
            session_names: names(&[NOTIFICATIONS]),
            portals: BTreeSet::new(),
            ..full()
        });
        let share = row(&rows, "share");
        assert_eq!(share.level, Level::Missing, "{desktop}");
        assert!(
            share.detail.contains(backend),
            "{desktop}: {}",
            share.detail
        );
    }
}

#[test]
fn x11_is_the_session_type_or_a_display_without_wayland() {
    for (kind, wayland, x11, expected) in [
        (Some("x11"), false, true, true),
        (Some("X11"), true, true, true),
        (Some("wayland"), false, true, false),
        // startx from a console login keeps logind's `tty`.
        (Some("tty"), false, true, true),
        (None, false, true, true),
        (Some(""), false, true, true),
        // A compositor started from a tty, with Xwayland.
        (Some("tty"), true, true, false),
        (None, false, false, false),
    ] {
        assert_eq!(
            x11_session(kind, wayland, x11),
            expected,
            "{kind:?} {wayland} {x11}"
        );
    }
}

#[test]
fn gnome_shell_extras_are_for_gnome_shell_only() {
    for desktop in ["GNOME", "ubuntu:GNOME", "pop:GNOME", "GNOME-Classic:GNOME"] {
        assert!(is_gnome_shell(desktop), "{desktop}");
    }
    for desktop in [
        "Budgie:GNOME",
        "GNOME-Flashback:GNOME",
        "KDE",
        "XFCE",
        "sway",
        "",
    ] {
        assert!(!is_gnome_shell(desktop), "{desktop}");
    }
    // Budgie has its own tray applet: no AppIndicator advice there.
    let rows = evaluate(&Probe {
        desktop: "Budgie:GNOME".into(),
        tray_host: false,
        ..full()
    });
    assert!(!row(&rows, "tray").detail.contains("AppIndicator"));
    assert!(row(&rows, "tray").install.is_none());
}

/// What each desktop gets, from the facts it really offers: every feature
/// either works or is said plainly not to (packaging/README.md, "Desktops").
#[test]
fn every_desktop_gets_each_feature_or_a_plain_reason() {
    use Level::{Limited as L, Missing as M, Ok as O};
    let wayland =
        |desktop: &str, portals: &[&str], sources: Option<u32>, capture: Option<u32>| Probe {
            desktop: desktop.into(),
            portals: names(portals),
            screencast_sources: sources,
            input_capture: capture,
            ..full()
        };
    let x11 = |desktop: &str, session: Option<&str>, portals: &[&str]| Probe {
        desktop: desktop.into(),
        session_type: session.map(str::to_string),
        wayland_display: false,
        x11_display: true,
        portals: names(portals),
        screencast_sources: None,
        input_capture: None,
        ..full()
    };
    let all = ["ScreenCast", "RemoteDesktop", "InputCapture", "Background"];
    let cases: [(&str, Probe, [Level; 5]); 11] = [
        // name, probe: share, extend, glide, unlock, autostart
        (
            "GNOME",
            wayland("GNOME", &all, Some(7), Some(3)),
            [O, O, O, O, O],
        ),
        (
            "Plasma 6.1+",
            wayland("KDE", &all, Some(7), Some(3)),
            [O, O, O, O, O],
        ),
        (
            "Plasma 6.0",
            wayland("KDE", &["ScreenCast", "RemoteDesktop"], Some(7), None),
            [O, O, M, O, O],
        ),
        (
            "Plasma on X11",
            x11("KDE", Some("x11"), &[]),
            [O, M, M, O, O],
        ),
        ("Xfce", x11("XFCE", Some("x11"), &[]), [O, M, M, L, O]),
        (
            "Cinnamon",
            x11("X-Cinnamon", Some("x11"), &[]),
            [O, M, M, L, O],
        ),
        ("MATE", x11("MATE", Some("x11"), &[]), [O, M, M, L, O]),
        (
            "sway",
            wayland("sway", &["ScreenCast"], Some(3), None),
            [L, M, M, L, L],
        ),
        (
            "Hyprland",
            wayland(
                "Hyprland",
                &["ScreenCast", "GlobalShortcuts"],
                Some(7),
                None,
            ),
            [L, M, M, L, L],
        ),
        (
            "COSMIC",
            wayland("COSMIC", &["ScreenCast"], Some(3), None),
            [L, M, M, L, O],
        ),
        (
            "i3 via startx",
            x11("i3", Some("tty"), &[]),
            [O, M, M, L, L],
        ),
    ];
    for (name, probe, expected) in cases {
        let rows = evaluate(&probe);
        let got: Vec<Level> = ["share", "extend", "glide", "unlock", "autostart"]
            .iter()
            .map(|id| level(&rows, id))
            .collect();
        assert_eq!(got, expected, "{name}");
        for row in rows.iter().filter(|row| row.level != Level::Ok) {
            assert!(row.detail.len() > 20, "{name}: {} says too little", row.id);
        }
    }
}

#[test]
fn a_wlroots_portal_without_remote_desktop_shares_view_only() {
    let rows = evaluate(&Probe {
        desktop: "sway".into(),
        portals: names(&["ScreenCast"]),
        ..full()
    });
    let share = row(&rows, "share");
    assert_eq!(share.level, Level::Limited, "view only");
    assert!(share.detail.starts_with(SHARE_THROUGH_DESKTOP));
    assert!(share
        .detail
        .contains("watch this screen but not control it"));
    assert!(share.tech.contains("RemoteDesktop"));
    // Not something to install: nothing would add remote control there.
    assert_eq!(share.install, None);
    // Watching still needs the PipeWire part and the encoder.
    let mut probe = Probe {
        desktop: "sway".into(),
        portals: names(&["ScreenCast"]),
        ..full()
    };
    probe.elements.remove("pipewiresrc");
    assert_eq!(level(&evaluate(&probe), "share"), Level::Missing);
}

#[test]
fn a_wayland_session_without_xwayland_says_copies_here_do_not_reach_the_phone() {
    assert_eq!(
        level(&evaluate(&full()), "clipboard"),
        Level::Ok,
        "GNOME has Xwayland"
    );
    let rows = evaluate(&Probe {
        desktop: "niri".into(),
        x11_display: false,
        ..full()
    });
    let clipboard = row(&rows, "clipboard");
    assert_eq!(clipboard.level, Level::Limited);
    assert!(clipboard.detail.contains("xwayland-satellite"));
    let x11 = Probe {
        session_type: Some("x11".into()),
        wayland_display: false,
        x11_display: true,
        ..full()
    };
    assert_eq!(level(&evaluate(&x11), "clipboard"), Level::Ok);
}

#[test]
fn another_listener_on_the_discovery_port_is_named_and_kept_in_view() {
    assert_eq!(level(&evaluate(&full()), "discovery"), Level::Ok);
    let rows = evaluate(&Probe {
        discovery_sharers: vec!["GSConnect".into(), "KDE Connect".into()],
        ..full()
    });
    let row = row(&rows, "discovery");
    assert_eq!(row.level, Level::Limited);
    assert!(row
        .detail
        .starts_with("GSConnect and KDE Connect also listen for"));
    assert!(row.detail.contains("Turn GSConnect and KDE Connect off"));
    let (id, words, command) = sticky_from(&rows, None).unwrap();
    assert_eq!((id, command), ("discovery", None), "a banner: {words}");
    assert_eq!(
        app_name("gjs", "gjs -m /usr/share/gnome-shell/extensions/gsconnect@andyholmes.github.io/service/daemon.js"),
        "GSConnect"
    );
    assert_eq!(
        app_name("kdeconnectd", "/usr/bin/kdeconnectd"),
        "KDE Connect"
    );
    assert_eq!(app_name("", ""), "Another program");
    assert_eq!(
        and_list(&["A".into(), "B".into(), "C".into()]),
        "A, B and C"
    );
}

/// F45: `desklink --check` while DeskLink runs listed the app itself
/// ("Turn desklink off"). The same program is never a rival; another
/// build of it (a Flatpak beside a native one) still is.
#[test]
fn the_running_app_is_not_counted_as_a_rival_by_its_own_check() {
    use std::path::Path;
    let me = Path::new("/home/u/.local/bin/desklink");
    assert!(is_this_app(7, 7, None, None));
    assert!(is_this_app(8, 7, Some(me), Some(me)));
    assert!(is_this_app(
        8,
        7,
        Some(Path::new("/home/u/.local/bin/desklink (deleted)")),
        Some(me)
    ));
    assert!(!is_this_app(
        8,
        7,
        Some(Path::new("/app/bin/desklink")),
        Some(me)
    ));
    assert!(!is_this_app(8, 7, None, Some(me)));
}

#[test]
fn the_proc_udp_table_gives_the_sockets_on_a_port() {
    let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n   23: 00000000:06B4 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 344901 2 00000000302a0e1b 0\n   24: 0100007F:06B4 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 5 2 00000000302a0e1b 0\n 2009: 0D01A8C0:0E76 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 168763 2 000000000295dcd8 0\n";
    assert_eq!(udp4_inodes(table, 1716), [344901, 5]);
    assert!(udp4_inodes(table, 1717).is_empty());
}

/// What the row is about, on this kernel: two IPv4 sockets sharing a
/// port with `SO_REUSEADDR` get each unicast datagram once between them
/// (a phone's beacon reaches one), and the probe sees the second holder.
/// A dual-stack IPv6 socket (GSConnect's) does not take IPv4 unicast from
/// an IPv4 one, so it is not counted.
#[test]
fn a_unicast_datagram_reaches_one_of_two_sockets_sharing_a_port() {
    use socket2::{Domain, Socket, Type};
    use std::net::{SocketAddr, UdpSocket};
    let bind = |domain: Domain, addr: SocketAddr| -> std::io::Result<UdpSocket> {
        let socket = Socket::new(domain, Type::DGRAM, None)?;
        socket.set_reuse_address(true)?;
        if domain == Domain::IPV6 {
            socket.set_only_v6(false)?;
        }
        socket.bind(&addr.into())?;
        Ok(socket.into())
    };
    let first = bind(Domain::IPV4, "0.0.0.0:0".parse().unwrap()).unwrap();
    let port = first.local_addr().unwrap().port();
    let second = bind(Domain::IPV4, SocketAddr::from(([0, 0, 0, 0], port))).unwrap();
    let wait = std::time::Duration::from_millis(300);
    let received = |sockets: &[&UdpSocket]| -> usize {
        UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .send_to(b"beacon", ("127.0.0.1", port))
            .unwrap();
        sockets
            .iter()
            .filter(|socket| {
                socket.set_read_timeout(Some(wait)).unwrap();
                socket.recv(&mut [0; 16]).is_ok()
            })
            .count()
    };
    assert_eq!(
        received(&[&first, &second]),
        1,
        "one of the two, never both"
    );
    let table = std::fs::read_to_string("/proc/net/udp").unwrap();
    let inodes = udp4_inodes(&table, port);
    assert_eq!(inodes.len(), 2);
    assert!(
        holders(&inodes, std::process::id(), None).is_empty(),
        "both are ours"
    );
    assert_eq!(
        holders(&inodes, 0, None).len(),
        1,
        "seen from outside: this program"
    );
    drop(second);
    // A dual-stack listener (where IPv6 exists) leaves IPv4 unicast to us.
    if let Ok(dual) = bind(Domain::IPV6, SocketAddr::from(([0u16; 8], port))) {
        first.set_read_timeout(Some(wait)).unwrap();
        UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .send_to(b"beacon", ("127.0.0.1", port))
            .unwrap();
        assert!(first.recv(&mut [0; 16]).is_ok(), "the IPv4 socket gets it");
        dual.set_read_timeout(Some(wait)).unwrap();
        assert!(dual.recv(&mut [0; 16]).is_err());
        let table = std::fs::read_to_string("/proc/net/udp").unwrap();
        assert_eq!(udp4_inodes(&table, port).len(), 1, "not in the IPv4 table");
    }
}

#[test]
fn a_session_type_of_x11_overrides_a_stray_wayland_display() {
    let rows = evaluate(&Probe {
        session_type: Some("X11".into()),
        x11_display: true,
        ..full()
    });
    assert!(row(&rows, "session").detail.contains("on X11"));
}

#[test]
fn missing_webrtc_plugins_are_named_with_packages_for_each_family() {
    let mut probe = full();
    probe.elements.remove("webrtcbin");
    probe.elements.remove("nicesrc");
    let rows = evaluate(&probe);
    let link = row(&rows, "link");
    assert_eq!(link.level, Level::Missing);
    assert!(link.tech.contains("webrtcbin") && link.tech.contains("nicesrc"));
    assert!(
        !link.detail.contains("webrtcbin"),
        "plain words for the person"
    );
    let packages = link.install.unwrap();
    assert!(packages.debian.contains("gstreamer1.0-plugins-bad"));
    assert!(packages.fedora.contains("gstreamer1-plugins-bad"));
    assert!(packages.arch.contains("gst-plugins-bad"));
    assert!(packages.opensuse.contains("gstreamer-plugins-bad"));
}

#[test]
fn sound_needs_pipewire_and_says_so_on_a_pulseaudio_only_machine() {
    let rows = evaluate(&Probe {
        pipewire_socket: false,
        pulse_socket: true,
        ..full()
    });
    let sound = row(&rows, "sound");
    assert_eq!(sound.level, Level::Missing);
    assert!(sound.detail.contains("older PulseAudio"));
    // A Flatpak sees the socket only with `xdg-run/pipewire-0`.
    let rows = evaluate(&Probe {
        pipewire_socket: false,
        flatpak: true,
        ..full()
    });
    assert_eq!(level(&rows, "sound"), Level::Missing);
    assert!(row(&rows, "sound").detail.contains("xdg-run/pipewire-0"));
    let rows = evaluate(&Probe {
        flatpak: true,
        ..full()
    });
    assert_eq!(level(&rows, "sound"), Level::Ok);
    let mut probe = full();
    probe.elements.remove("opusenc");
    assert_eq!(level(&evaluate(&probe), "sound"), Level::Missing);
}

#[test]
fn without_logind_locking_still_has_a_fallback() {
    let rows = evaluate(&Probe {
        system_names: BTreeSet::new(),
        system_bus: false,
        ..full()
    });
    let lock = row(&rows, "lock");
    assert_eq!(lock.level, Level::Limited);
    assert!(lock.detail.contains("elogind"));
}

#[test]
fn extend_is_offered_only_on_gnome_and_kde_even_if_a_portal_claims_virtual() {
    for (desktop, expected) in [
        ("GNOME", Level::Ok),
        ("KDE", Level::Ok),
        ("Hyprland", Level::Missing),
        ("sway", Level::Missing),
        ("COSMIC", Level::Missing),
    ] {
        let rows = evaluate(&Probe {
            desktop: desktop.into(),
            ..full()
        });
        assert_eq!(level(&rows, "extend"), expected, "{desktop}");
    }
}

#[test]
fn extend_on_a_plasma_whose_portal_has_no_virtual_screen_names_the_portal_not_plasma() {
    // Plasma 6.3 (Debian 13): ScreenCast offers monitors and windows only.
    let rows = evaluate(&Probe {
        desktop: "KDE".into(),
        screencast_sources: Some(3),
        ..full()
    });
    let extend = row(&rows, "extend");
    assert_eq!(extend.level, Level::Missing);
    assert!(
        !extend.detail.contains("KDE Plasma can"),
        "{}",
        extend.detail
    );
    assert!(
        extend.detail.contains("xdg-desktop-portal-kde"),
        "{}",
        extend.detail
    );
}

#[test]
fn extend_and_glide_follow_the_portal_not_the_desktop_name() {
    let rows = evaluate(&Probe {
        desktop: "sway".into(),
        screencast_sources: Some(3),
        input_capture: None,
        portals: names(&["ScreenCast", "RemoteDesktop"]),
        ..full()
    });
    assert_eq!(level(&rows, "extend"), Level::Missing);
    assert_eq!(level(&rows, "glide"), Level::Missing);
    let rows = evaluate(&Probe {
        desktop: "KDE".into(),
        ..full()
    });
    assert_eq!(level(&rows, "extend"), Level::Ok);
    assert_eq!(level(&rows, "glide"), Level::Ok);
}

#[test]
fn no_session_bus_no_display_no_runtime_dir_and_a_read_only_home_are_each_said() {
    let rows = evaluate(&Probe {
        session_bus: false,
        session_names: BTreeSet::new(),
        portals: BTreeSet::new(),
        tray_host: false,
        runtime_dir: false,
        config_writable: false,
        wayland_display: false,
        x11_display: false,
        ..full()
    });
    assert_eq!(level(&rows, "bus"), Level::Missing);
    assert!(row(&rows, "bus").tech.contains("dbus-run-session"));
    assert_eq!(level(&rows, "session"), Level::Missing);
    assert_eq!(level(&rows, "storage"), Level::Missing);
    assert_eq!(level(&rows, "runtime"), Level::Limited);
    assert_eq!(level(&rows, "share"), Level::Missing);
}

#[test]
fn no_terminal_program_is_limited_except_in_a_flatpak() {
    let none = Probe {
        terminals: BTreeSet::new(),
        ..full()
    };
    assert_eq!(level(&evaluate(&none), "terminal"), Level::Limited);
    let flatpak = Probe {
        flatpak: true,
        ..none
    };
    assert_eq!(level(&evaluate(&flatpak), "terminal"), Level::Ok);
}

#[test]
fn video_decoders_and_encoders_are_each_required_somewhere() {
    let mut probe = full();
    for decoder in ["vp8dec", "avdec_h264", "openh264dec", "vah264dec"] {
        probe.elements.remove(decoder);
    }
    let rows = evaluate(&probe);
    assert_eq!(level(&rows, "view"), Level::Missing);
    assert_eq!(level(&rows, "share"), Level::Ok, "encoders are separate");
}

#[test]
fn os_release_maps_to_a_family_and_a_command() {
    let id = |text| distro_from_os_release(text);
    assert_eq!(id("ID=ubuntu\nID_LIKE=debian\n"), Some(Distro::Debian));
    assert_eq!(
        id("ID=\"linuxmint\"\nID_LIKE=\"ubuntu debian\""),
        Some(Distro::Debian)
    );
    assert_eq!(id("ID=fedora\n"), Some(Distro::Fedora));
    assert_eq!(
        id("ID=rocky\nID_LIKE=\"rhel centos fedora\""),
        Some(Distro::Fedora)
    );
    assert_eq!(id("ID=manjaro\nID_LIKE=arch"), Some(Distro::Arch));
    assert_eq!(id("ID=endeavouros\nID_LIKE=arch"), Some(Distro::Arch));
    assert_eq!(
        id("NAME=x\nID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\""),
        Some(Distro::Suse)
    );
    assert_eq!(id("ID=nixos"), Some(Distro::Nix));
    assert_eq!(id("ID=void"), None);
    assert_eq!(id(""), None);
    assert_eq!(
        GST_BAD.command(Some(Distro::Debian)),
        "sudo apt install gstreamer1.0-plugins-bad gstreamer1.0-nice"
    );
    assert_eq!(
        GST_BAD.command(Some(Distro::Arch)),
        "sudo pacman -S gst-plugins-bad libnice"
    );
    assert!(GST_BAD.command(Some(Distro::Nix)).contains("NixOS"));
    assert!(GST_BAD.command(None).contains("Fedora:"));
}

#[test]
fn unlock_is_promised_only_where_the_lock_screen_is_known_to_obey() {
    for (desktop, expected) in [
        ("GNOME", Level::Ok),
        ("ubuntu:GNOME", Level::Ok),
        ("KDE", Level::Ok),
        ("XFCE", Level::Limited),
        ("sway", Level::Limited),
        ("", Level::Limited),
    ] {
        let rows = evaluate(&Probe {
            desktop: desktop.into(),
            ..full()
        });
        assert_eq!(level(&rows, "unlock"), expected, "{desktop:?}");
    }
}

#[test]
fn a_flatpak_on_x11_without_a_portal_cannot_capture_and_says_so() {
    let mut probe = Probe {
        flatpak: true,
        desktop: "X-Cinnamon".into(),
        wayland_display: false,
        x11_display: true,
        session_type: Some("x11".into()),
        session_names: names(&[NOTIFICATIONS]),
        portals: BTreeSet::new(),
        ..full()
    };
    probe.elements.remove("ximagesrc");
    let rows = evaluate(&probe);
    let share = row(&rows, "share");
    assert_eq!(share.level, Level::Missing);
    assert!(share.detail.contains("Flatpak"));
    assert!(share.install.is_none());
}

#[test]
fn the_text_report_lists_every_row_and_one_command() {
    let rows = evaluate(&Probe::default());
    let text = as_text(&rows, Some(Distro::Fedora));
    for row in &rows {
        assert!(text.contains(row.title), "{}", row.title);
    }
    assert!(text.contains("[missing] Phone link"));
    assert!(text.contains("sudo dnf install gstreamer1-plugins-bad-free"));
    assert!(!text.contains("sudo apt"));
    assert!(as_text(&rows, None).contains("Arch: gst-plugins-bad libnice"));
}

#[test]
fn the_dialog_text_lists_every_family() {
    let lines = GST_BAD.lines();
    for family in ["Debian/Ubuntu:", "Fedora:", "Arch:", "openSUSE:"] {
        assert!(lines.contains(family));
    }
}

#[test]
fn every_row_has_a_unique_id_and_plain_text() {
    let rows = evaluate(&Probe::default());
    let mut ids: Vec<_> = rows.iter().map(|row| row.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), rows.len());
    for row in rows {
        assert!(!row.detail.is_empty() && !row.title.is_empty());
    }
}

/// The person reads plain words; component and service names stay in the
/// technical note (the copied report, `--check` and the log).
#[test]
fn details_name_no_components_and_the_report_keeps_them() {
    let jargon = [
        "GStreamer",
        "WebRTC",
        "portal",
        "D-Bus",
        "StatusNotifier",
        "XDG_",
        "webrtcbin",
        "vp8",
        "VP8",
        "pipewiresrc",
        "logind's",
        "virtual monitor",
    ];
    let mut machines = vec![Probe::default(), full()];
    let mut bare = full();
    bare.elements.clear();
    bare.portals.clear();
    bare.tray_host = false;
    bare.session_names = names(&[PORTAL]);
    machines.push(bare);
    machines.push(Probe {
        desktop: "sway".into(),
        portals: names(&["ScreenCast"]),
        ..full()
    });
    machines.push(Probe {
        flatpak: true,
        pipewire_socket: false,
        ..full()
    });
    for probe in &machines {
        let rows = evaluate(probe);
        for row in &rows {
            // A package to install is named as the distribution names it.
            let words = row.detail.replace("xdg-desktop-portal", "");
            for word in jargon {
                assert!(
                    !words.contains(word),
                    "{}: {:?} in {:?}",
                    row.id,
                    word,
                    row.detail
                );
            }
        }
        let text = as_text(&rows, None);
        for row in rows.iter().filter(|row| !row.tech.is_empty()) {
            assert!(text.contains(&row.tech), "{}", row.id);
        }
    }
}

#[test]
fn writability_is_asked_of_the_nearest_existing_folder_and_writes_nothing() {
    // As root everything is writable: the body runs as `nobody` there.
    if !crate::test_privileges::unprivileged(
        "platform::tests::writability_is_asked_of_the_nearest_existing_folder_and_writes_nothing",
    ) {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("desklink-writable-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let below = dir.join("not").join("made").join("yet");
    assert!(writable_below(&below));
    assert!(!below.exists(), "nothing was created");
    // A folder the user may not write to, and one that is not there at all.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    assert!(!writable_below(&below));
    assert!(!writable_below(std::path::Path::new("/proc/self/nothing")));
    assert!(!writable_below(std::path::Path::new("")));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn only_a_definite_no_from_d_bus_switches_a_portal_feature_off() {
    for no in [
        "org.freedesktop.DBus.Error.UnknownMethod",
        "org.freedesktop.DBus.Error.UnknownInterface",
        "org.freedesktop.DBus.Error.UnknownProperty",
        "org.freedesktop.DBus.Error.UnknownObject",
        "org.freedesktop.DBus.Error.InvalidArgs",
    ] {
        assert!(definite_negative(no), "{no}");
    }
    for maybe in [
        "org.freedesktop.DBus.Error.NoReply",
        "org.freedesktop.DBus.Error.Timeout",
        "org.freedesktop.DBus.Error.TimedOut",
        "org.freedesktop.DBus.Error.ServiceUnknown",
        "org.freedesktop.DBus.Error.Failed",
        "",
    ] {
        assert!(!definite_negative(maybe), "{maybe}");
    }
}

#[test]
fn a_name_is_absent_only_when_both_bus_lists_were_read_and_lack_it() {
    let watched = [PORTAL, NOTIFICATIONS];
    // Both lists read: what is not there is not there.
    let complete = settle_names(Some(names(&[PORTAL])), Some(names(&["x"])), &watched);
    assert!(complete.contains(PORTAL) && !complete.contains(NOTIFICATIONS));
    // Activatable but not running counts as present.
    let activatable = settle_names(Some(names(&[])), Some(names(&[NOTIFICATIONS])), &watched);
    assert!(activatable.contains(NOTIFICATIONS));
    // One list missing (a timeout): every watched name is believed present.
    for (listed, activatable) in [
        (None, Some(names(&[]))),
        (Some(names(&[])), None),
        (None, None),
    ] {
        let all = settle_names(listed, activatable, &watched);
        assert!(all.contains(PORTAL) && all.contains(NOTIFICATIONS));
    }
}

#[test]
fn the_session_bus_is_asked_for_only_where_that_starts_nothing() {
    assert!(session_bus_reachable(true, false));
    assert!(session_bus_reachable(false, true));
    assert!(!session_bus_reachable(false, false));
}

#[test]
fn the_banner_is_for_the_two_losses_that_stop_everything() {
    let mut probe = full();
    assert_eq!(sticky_from(&evaluate(&probe), None), None);
    probe.config_writable = false;
    let (id, words, command) = sticky_from(&evaluate(&probe), None).unwrap();
    assert_eq!(id, "storage");
    assert!(words.contains("configuration folder"));
    assert_eq!(command, None);
    probe.elements.remove("webrtcbin");
    let (id, words, command) = sticky_from(&evaluate(&probe), Some(Distro::Arch)).unwrap();
    assert_eq!(id, "link", "the phone link comes first");
    assert!(
        !words.contains("sudo"),
        "the command is the button's: {words}"
    );
    assert!(
        command.is_some_and(|command| command.contains("sudo pacman -S gst-plugins-bad libnice")),
    );
    // A limit (no tray, no notifier) is never a banner.
    let probe = Probe {
        tray_host: false,
        session_names: BTreeSet::new(),
        ..full()
    };
    assert_eq!(sticky_from(&evaluate(&probe), None), None);
}

#[test]
fn a_later_probe_replaces_the_report_and_counts() {
    let _held = TEST_REPORT.lock().unwrap_or_else(|e| e.into_inner());
    let before = generation();
    set_report_for_tests(evaluate(&Probe::default()));
    assert!(generation() > before);
    assert!(report().is_some());
    assert!(!works("link"));
    set_report_for_tests(evaluate(&full()));
    assert!(works("link"));
    assert_eq!(reason("sound"), None);
    clear_report_for_tests();
    assert!(works("link"), "nothing is hidden before a probe");
}

#[test]
fn the_share_row_needs_the_vp8_encoder_not_any_encoder() {
    let mut probe = full();
    probe.elements.remove("vp8enc");
    assert_eq!(level(&evaluate(&probe), "share"), Level::Missing);
    assert!(row(&evaluate(&probe), "share").tech.contains("vp8enc"));
    probe.elements.insert("x264enc".into());
    probe.elements.insert("vah264enc".into());
    assert_eq!(level(&evaluate(&probe), "share"), Level::Missing);
}
