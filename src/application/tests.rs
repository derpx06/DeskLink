//! Tests of the application: Bucket questions, crash reports, colour contrast of the palette, and session lines.

fn ask(device: &str, bucket: bool) -> crate::device_links::folders::Offer {
    crate::device_links::folders::Offer {
        device: device.into(),
        id: "bucket".into(),
        name: "Bucket".into(),
        bucket,
    }
}

/// A phone asking for a Bucket is told about once; asking again after the
/// question went away is a new question; a folder offer is not a Bucket.
#[test]
fn a_bucket_question_is_notified_once_per_ask() {
    let mut told = std::collections::HashSet::new();
    let first = new_bucket_questions(&[ask("p1", true), ask("p2", false)], &mut told);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].device, "p1");
    // Still asked on the next tick: not again.
    assert!(new_bucket_questions(&[ask("p1", true)], &mut told).is_empty());
    // Answered (the offer is gone), then asked again later.
    assert!(new_bucket_questions(&[], &mut told).is_empty());
    assert_eq!(new_bucket_questions(&[ask("p1", true)], &mut told).len(), 1);
}

use super::*;

#[test]
fn a_crash_is_reported_once() {
    let dir = std::env::temp_dir().join(format!("desklink-crash-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    assert!(!take_unreported_crash(&dir), "no crash");
    std::fs::write(dir.join("crash-1700000000.txt"), b"x").unwrap();
    assert!(take_unreported_crash(&dir));
    assert!(!take_unreported_crash(&dir), "told already");
    let _ = std::fs::remove_dir_all(dir);
}

/// "#rrggbb" or "rgba(r,g,b,a)" as linear-light-ready 0..1 channels and alpha.
fn parse(colour: &str) -> ([f64; 3], f64) {
    if let Some(hex) = colour.strip_prefix('#') {
        let channel =
            |at: usize| f64::from(u8::from_str_radix(&hex[at..at + 2], 16).unwrap()) / 255.0;
        return ([channel(0), channel(2), channel(4)], 1.0);
    }
    let inner = colour.trim_start_matches("rgba(").trim_end_matches(')');
    let parts: Vec<f64> = inner
        .split(',')
        .map(|part| part.trim().parse().unwrap())
        .collect();
    (
        [parts[0] / 255.0, parts[1] / 255.0, parts[2] / 255.0],
        parts[3],
    )
}

fn colour(name: &str, dark: bool) -> &'static str {
    let (_, light, dark_value) = PALETTE
        .iter()
        .chain(TOKENS)
        .find(|(token, _, _)| *token == name)
        .unwrap_or_else(|| panic!("no token {name}"));
    if dark {
        dark_value
    } else {
        light
    }
}

/// `name` painted over `under` (an opaque colour).
fn composite(name: &str, under: [f64; 3], dark: bool) -> [f64; 3] {
    let (rgb, alpha) = parse(colour(name, dark));
    [0, 1, 2].map(|i| rgb[i] * alpha + under[i] * (1.0 - alpha))
}

fn luminance(rgb: [f64; 3]) -> f64 {
    let linear = |c: f64| {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(rgb[0]) + 0.7152 * linear(rgb[1]) + 0.0722 * linear(rgb[2])
}

fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// WCAG AA: every text colour reads at 4.5:1 or more on each surface it
/// is used on, light and dark.
/// The accent goes in its own stylesheet, above the user's; the rest of
/// the palette does not.
#[test]
fn the_accent_is_split_out_of_the_palette() {
    let palette = palette_css(false, true, false);
    let accent = accent_css(false, true, false);
    assert!(!palette.contains("accent_bg_color"), "{palette}");
    assert!(accent.contains("@define-color accent_bg_color #3b63e6;"));
    assert!(accent.contains("--accent-bg-color: #3b63e6;"));
    assert!(accent_css(true, true, false).contains("--accent-bg-color: #7c95ff;"));
    assert!(
        accent_css(false, true, true).is_empty(),
        "high contrast keeps its own"
    );
}

#[test]
fn every_text_colour_meets_4_5_to_1_on_its_surfaces() {
    let text = [
        "window_fg_color",
        "text_dim",
        "text_faint",
        "accent_color",
        "coral_text",
        "danger",
    ];
    let surfaces = ["window_bg_color", "sidebar_bg_color", "card_bg_color"];
    for dark in [false, true] {
        let solid = |name: &str| parse(colour(name, dark)).0;
        let card = solid("card_bg_color");
        let mut pairs: Vec<(String, [f64; 3], String, [f64; 3])> = Vec::new();
        for fg in text {
            for bg in surfaces {
                pairs.push((fg.into(), solid(fg), bg.into(), solid(bg)));
            }
        }
        // Buttons and chips: a soft fill on a card.
        let chip = composite("surface_2", card, dark);
        for fg in ["window_fg_color", "text_dim", "accent_color", "coral_text"] {
            pairs.push((fg.into(), solid(fg), "surface_2 on card".into(), chip));
        }
        for (fg, bg) in [
            ("accent_fg_color", "accent_bg_color"),
            ("destructive_fg_color", "destructive_bg_color"),
            ("avatar_fg", "avatar_bg"),
            ("window_fg_color", "coral_bg"),
            ("text_dim", "coral_bg"),
            ("coral_text", "coral_bg"),
            ("terminal_fg", "terminal_bg"),
            ("terminal_lines", "terminal_bg"),
            ("terminal_dim", "terminal_bg"),
            ("terminal_prompt", "terminal_bg"),
            ("terminal_coral", "terminal_bg"),
        ] {
            pairs.push((fg.into(), solid(fg), bg.into(), solid(bg)));
        }
        pairs.push((
            "danger".into(),
            solid("danger"),
            "danger_bg on card".into(),
            composite("danger_bg", card, dark),
        ));
        for (fg, fg_rgb, bg, bg_rgb) in pairs {
            let ratio = contrast(fg_rgb, bg_rgb);
            assert!(
                ratio >= 4.5,
                "{fg} on {bg} ({}) is {ratio:.2}:1",
                if dark { "dark" } else { "light" }
            );
        }
    }
}

/// An Insert waiting on the phone shows in the indicator too, so the tray
/// and the notification offer to end it.
#[test]
fn the_session_line_includes_a_capture_waiting_on_the_phone() {
    use crate::device_links::device::{CaptureUse, DeviceView};
    let info = crate::device_links::device_info::DeviceInfo::local("p".into(), "Pixel".into());
    let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
    device.capture = Some(CaptureUse {
        kind: "scan".into(),
        receiving: false,
    });
    assert_eq!(
        session_line(&device).as_deref(),
        Some("Pixel — waiting for a scan")
    );
    device.capture = Some(CaptureUse {
        kind: "photo".into(),
        receiving: true,
    });
    assert_eq!(
        session_line(&device).as_deref(),
        Some("Pixel — waiting for a photo")
    );
}

/// The indicator also says when this computer's sound or microphone goes
/// to a phone, or the phone's sound plays here (not only the screen and
/// terminals), so a closed window does not hide it.
#[test]
fn the_session_line_includes_sound() {
    use crate::device_links::device::{DeviceView, SoundUse};
    let info = crate::device_links::device_info::DeviceInfo::local("p".into(), "Pixel".into());
    let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
    assert_eq!(session_line(&device), None);
    device.sound = Some(SoundUse {
        sending: Some("mic".into()),
        hearing: None,
        hearing_live: false,
    });
    assert_eq!(
        session_line(&device).as_deref(),
        Some("Pixel — sending its microphone")
    );
    device.sound = Some(SoundUse {
        sending: Some("system".into()),
        hearing: None,
        hearing_live: false,
    });
    assert_eq!(
        session_line(&device).as_deref(),
        Some("Pixel — sending its sound")
    );
    device.sound = Some(SoundUse {
        sending: None,
        hearing: Some("system".into()),
        hearing_live: true,
    });
    device.terminal_sessions_attached = 2;
    assert_eq!(
        session_line(&device).as_deref(),
        Some("Pixel — using 2 terminals, playing the phone's sound")
    );
    // Asked for but not flowing yet: nothing to indicate.
    device.terminal_sessions_attached = 0;
    device.sound = Some(SoundUse {
        sending: None,
        hearing: Some("system".into()),
        hearing_live: false,
    });
    assert_eq!(session_line(&device), None);
}
