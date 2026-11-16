//! Which activities Home shows, and in what order.

use super::*;
use crate::device_links::activity::snapshot_from;

fn device_with(items: serde_json::Value, at: i64, now: i64) -> DeviceView {
    let info = crate::device_links::device_info::DeviceInfo::local("p".into(), "Pixel".into());
    let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
    device.feature_transport_ready = true;
    let mut body = serde_json::Map::new();
    body.insert("items".into(), items);
    body.insert("at".into(), serde_json::json!(at));
    device.activity = Some(snapshot_from(&body, now));
    device
}

fn labels(root: &gtk::Widget, prefix: &str) -> Vec<String> {
    let mut found: Vec<_> = named_widgets(root, prefix)
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Label>().ok())
        .map(|l| (l.widget_name().to_string(), l.label().to_string()))
        .collect();
    found.sort();
    found.into_iter().map(|(_, label)| label).collect()
}

/// The banners that say what is wrong with the connection wrap onto more
/// lines, never end in an ellipsis: a long phone name must not hide the
/// reason or the way out. (Needs a display.)
#[test]
fn the_connection_banners_wrap_instead_of_cutting_their_words() {
    crate::test_display::run(|| {
        let info = crate::device_links::device_info::DeviceInfo::local(
            "p".into(),
            "A phone with a name that goes on and on and on".into(),
        );
        let device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
        let banner = reconnecting_row(&device);
        let labels = named_widgets(banner.upcast_ref(), "")
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::Label>().ok())
            .filter(|label| label.label().chars().count() > 12)
            .collect::<Vec<_>>();
        assert!(!labels.is_empty());
        for label in labels {
            assert!(label.wraps(), "{} wraps", label.label());
            assert_eq!(
                label.ellipsize(),
                gtk::pango::EllipsizeMode::None,
                "{} is not cut",
                label.label()
            );
        }
    });
}

/// The card names the phone, shows each item, and its countdown and bar
/// move in place; nothing ongoing, or a hidden phone, draws no card.
/// (Needs a display.)
#[test]
fn the_activity_card_shows_items_and_counts_down_in_place() {
    crate::test_display::run(|| {
        let at = crate::device_links::activity::now_ms();
        let now = at;
        let device = device_with(
            serde_json::json!([
                {"id": "t", "appLabel": "Clock", "title": "Pasta", "kind": "timer",
                 "chronometerBase": at + 272_000, "countdown": true},
                {"id": "d", "appLabel": "Files", "title": "Download", "kind": "progress",
                 "progress": 40}
            ]),
            at,
            now,
        );
        let settings = crate::device_links::config::AppSettings::default();
        let card = activity_card(&device, &settings).expect("a card");
        let root = card.clone().upcast::<gtk::Widget>();
        paint_activities(&root, &device, &settings, now);
        assert_eq!(labels(&root, "activity-live:"), ["4:32 left", "40%"]);
        paint_activities(&root, &device, &settings, now + 10_000);
        assert_eq!(labels(&root, "activity-live:")[0], "4:22 left", "in place");
        let bars = named_widgets(&root, "activity-bar:");
        assert_eq!(bars.len(), 1);
        let bar = bars[0].downcast_ref::<gtk::ProgressBar>().unwrap();
        assert!((bar.fraction() - 0.4).abs() < 1e-9);

        let mut hidden = settings.clone();
        hidden.hidden_notifications.insert("p".into());
        assert!(activity_card(&device, &hidden).is_none());
        let mut offline = device.clone();
        offline.feature_transport_ready = false;
        assert!(activity_card(&offline, &settings).is_none());
        let empty = device_with(serde_json::json!([]), at, now);
        assert!(activity_card(&empty, &settings).is_none());
    });
}
