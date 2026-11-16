//! Home's now-playing bar: position, seek, controls and the cover art.
//!
//! Sibling edges: none. Outside: `media_art`, `media_seek`.

use super::*;

/// The phone's music while it plays: track, progress and controls.
pub(super) fn now_playing_bar(daemon: &DaemonHandle, device: &DeviceView) -> Option<gtk::Box> {
    // Not while the link is down: what the phone last said is stale.
    if !device.feature_transport_ready {
        return None;
    }
    // Paused too: hiding the bar on Pause left no button to play again.
    let media = device
        .media_status
        .as_ref()
        .filter(|media| media.is_playing || !media.title.is_empty())?;
    let bar = gtk::Box::builder()
        .spacing(16)
        .css_classes(["dl-card", "dl-card-new", "now-playing"])
        .build();
    bar.append(&art(52, media.album_art.as_ref()));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(3)
        .width_request(200)
        .valign(gtk::Align::Center)
        .build();
    words.append(&ellipsized(
        if media.title.is_empty() {
            "Playing"
        } else {
            &media.title
        },
        &["heading"],
    ));
    let state = if media.is_playing {
        "playing"
    } else {
        "paused"
    };
    let source = if media.artist.is_empty() {
        format!("{state} on {}", device.name)
    } else {
        format!("{} · {state} on {}", media.artist, device.name)
    };
    words.append(&ellipsized(&source, &["caption", "text-dim"]));
    bar.append(&words);
    let progress = gtk::Box::builder()
        .spacing(10)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    if let Some(length) = media.length.filter(|l| *l > 0) {
        let at = shown_position(media, std::time::Instant::now());
        let pos = text(&clock(at), &["mono", "caption", "text-faint"]);
        pos.set_widget_name(&format!("media-pos:{}", device.id));
        progress.append(&pos);
        progress.append(&seek_scale(daemon, device, media, &pos, at, length));
        progress.append(&text(&clock(length), &["mono", "caption", "text-faint"]));
    }
    bar.append(&progress);
    bar.append(&media_controls(
        daemon,
        device,
        &media.player,
        media.is_playing,
    ));
    Some(bar)
}

pub(super) fn clock(ms: i64) -> String {
    format!("{}:{:02}", ms / 60_000, (ms / 1000) % 60)
}

/// Where the phone's track is now: the last report, moved on while it plays.
pub(super) fn shown_position(
    media: &crate::device_links::device::MediaStatus,
    now: std::time::Instant,
) -> i64 {
    let position = media.position.unwrap_or(0);
    match media.position_at {
        Some(at) => crate::device_links::media_seek::now_at(
            position,
            at,
            now,
            media.is_playing,
            media.length,
        ),
        None => position,
    }
}

/// The track's timeline: drag or click to seek the phone's player. Values go
/// out at most five a second while dragging and always on release; the bar
/// is dimmed, with the reason as its tooltip, when the player cannot seek.
fn seek_scale(
    daemon: &DaemonHandle,
    device: &DeviceView,
    media: &crate::device_links::device::MediaStatus,
    pos: &gtk::Label,
    at: i64,
    length: i64,
) -> gtk::Scale {
    use crate::device_links::media_seek::{blocked, Throttle, DRAG_EVERY};
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, length as f64, 1000.0);
    scale.set_draw_value(false);
    scale.set_hexpand(true);
    scale.set_valign(gtk::Align::Center);
    scale.set_value(at as f64);
    scale.add_css_class("track");
    scale.set_widget_name(&format!("media-seek:{}", device.id));
    scale.update_property(&[gtk::accessible::Property::Label("Track position")]);
    let linked =
        can_send_feature(device, PACKET_TYPE_MPRIS_REQUEST) && device.feature_transport_ready;
    let reason = blocked(media.can_seek, media.length).or(if linked {
        None
    } else {
        Some("Waiting for the phone")
    });
    scale.set_sensitive(reason.is_none());
    scale.set_tooltip_text(reason);
    let state = Rc::new(RefCell::new((Throttle::default(), None::<i64>)));
    let scheduled = Rc::new(Cell::new(false));
    let send = {
        let (daemon, id, player) = (daemon.clone(), device.id.clone(), media.player.clone());
        Rc::new(move |ms: i64| {
            daemon.send(DaemonCommand::SendMprisSetPosition(
                id.clone(),
                player.clone(),
                ms,
            ))
        })
    };
    {
        let (state, scheduled, send, pos) =
            (state.clone(), scheduled.clone(), send.clone(), pos.clone());
        scale.connect_change_value(move |_, _, value| {
            let ms = (value.round() as i64).clamp(0, length);
            pos.set_label(&clock(ms));
            let due = {
                let mut state = state.borrow_mut();
                state.1 = Some(ms);
                state.0.due(std::time::Instant::now())
            };
            if due {
                state.borrow_mut().1 = None;
                send(ms);
            } else if !scheduled.replace(true) {
                let (state, scheduled, send) = (state.clone(), scheduled.clone(), send.clone());
                gtk::glib::timeout_add_local_once(DRAG_EVERY, move || {
                    scheduled.set(false);
                    let pending = state.borrow_mut().1.take();
                    if let Some(ms) = pending {
                        state.borrow_mut().0.force(std::time::Instant::now());
                        send(ms);
                    }
                });
            }
            gtk::glib::Propagation::Proceed
        });
    }
    // The window leaves the bar alone while it is held (the page is redrawn
    // with the phone's position as the finger moves otherwise).
    let click = gtk::GestureClick::new();
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let scale = scale.clone();
        click.connect_pressed(move |_, _, _, _| scale.add_css_class("seeking"));
    }
    {
        let (scale, state, send) = (scale.clone(), state.clone(), send.clone());
        click.connect_released(move |_, _, _, _| {
            scale.remove_css_class("seeking");
            let pending = state.borrow_mut().1.take();
            if let Some(ms) = pending {
                state.borrow_mut().0.force(std::time::Instant::now());
                send(ms);
            }
        });
    }
    scale.add_controller(click);
    scale
}

/// Previous, play/pause and next for the phone's player.
fn media_controls(
    daemon: &DaemonHandle,
    device: &DeviceView,
    player: &str,
    playing: bool,
) -> gtk::Box {
    let controls = gtk::Box::builder()
        .spacing(4)
        .valign(gtk::Align::Center)
        .build();
    let can = can_send_feature(device, PACKET_TYPE_MPRIS_REQUEST) && device.feature_transport_ready;
    for (icon, label, action, class) in [
        (
            "media-skip-backward-symbolic",
            "Previous",
            "Previous",
            "flat",
        ),
        (
            if playing {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            },
            if playing { "Pause" } else { "Play" },
            "PlayPause",
            "play-button",
        ),
        ("media-skip-forward-symbolic", "Next", "Next", "flat"),
    ] {
        let button = gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(label)
            .valign(gtk::Align::Center)
            .sensitive(can)
            .css_classes(["circular", class])
            .build();
        button.update_property(&[gtk::accessible::Property::Label(label)]);
        let (daemon, id, player) = (daemon.clone(), device.id.clone(), player.to_string());
        button.connect_clicked(move |_| {
            daemon.send(DaemonCommand::SendMprisAction(
                id.clone(),
                player.clone(),
                action.to_string(),
            ))
        });
        controls.append(&button);
    }
    controls
}

/// Album art stand-in: a soft gradient tile (the phone sends no artwork).
/// The cover, or the gradient square when there is none. The pixels were
/// decoded on the daemon thread; wrapping them is a copy-free texture.
fn art(size: i32, cover: Option<&crate::device_links::media_art::PhoneArt>) -> gtk::Box {
    let art = gtk::Box::builder()
        .width_request(size)
        .height_request(size)
        .valign(gtk::Align::Center)
        .css_classes(["album-art"])
        .accessible_role(gtk::AccessibleRole::Presentation)
        .overflow(gtk::Overflow::Hidden)
        .build();
    art.set_hexpand(false);
    if let Some(cover) = cover {
        let format = if cover.has_alpha {
            gtk::gdk::MemoryFormat::R8g8b8a8
        } else {
            gtk::gdk::MemoryFormat::R8g8b8
        };
        let texture = gtk::gdk::MemoryTexture::new(
            cover.width,
            cover.height,
            format,
            &cover.pixels,
            cover.stride,
        );
        let picture = gtk::Picture::builder()
            .paintable(&texture)
            .content_fit(gtk::ContentFit::Cover)
            .hexpand(true)
            .vexpand(true)
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build();
        art.append(&picture);
    }
    art
}
