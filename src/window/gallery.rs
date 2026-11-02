//! "From your phone" (W12): the latest photos and screenshots a phone sent on
//! its own, at the top of the Files page. Each has Copy, Open, Save and Remove.

use std::path::{Path, PathBuf};

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use super::{text, toast_from, DesklinkWindow};
use crate::device_links::device::DeviceView;
use crate::device_links::gallery::{self, Item, STRIP_ITEMS};

const THUMB_W: i32 = 160;
const THUMB_H: i32 = 120;

/// The strip for `device`, or `None` while photos are off for it or nothing is
/// kept.
pub(super) fn strip(device: &DeviceView) -> Option<gtk::Box> {
    let on = crate::device_links::config::app_settings().accepts_gallery_from(&device.id);
    let items: Vec<Item> = gallery::list(&device.id)
        .into_iter()
        .take(STRIP_ITEMS)
        .collect();
    build(device, on, &items, dirs::download_dir())
}

fn build(
    device: &DeviceView,
    on: bool,
    items: &[Item],
    downloads: Option<PathBuf>,
) -> Option<gtk::Box> {
    if !on || items.is_empty() {
        return None;
    }
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .css_classes(["dl-card"])
        .build();
    card.append(&text(
        &format!("From your phone · {}", device.name),
        &["heading"],
    ));
    let row = gtk::Box::builder().spacing(12).build();
    for item in items {
        row.append(&tile(item, downloads.clone()));
    }
    card.append(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .child(&row)
            .build(),
    );
    Some(card)
}

fn tile(item: &Item, downloads: Option<PathBuf>) -> gtk::Box {
    let tile = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .width_request(THUMB_W)
        .build();
    let picture = gtk::Picture::builder()
        .width_request(THUMB_W)
        .height_request(THUMB_H)
        .content_fit(gtk::ContentFit::Cover)
        .css_classes(["file-preview"])
        .build();
    picture.set_accessible_role(gtk::AccessibleRole::Img);
    picture.update_property(&[gtk::accessible::Property::Label(&item.name)]);
    load_thumbnail(&picture, item.path.clone());
    tile.append(&picture);
    let name = super::ellipsized(&item.name, &["caption", "text-dim"]);
    name.set_max_width_chars(20);
    tile.append(&name);
    let buttons = gtk::Box::builder().spacing(2).build();
    let make = |icon: &str, label: &str, run: Box<dyn Fn(&gtk::Button)>| {
        let button = gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(label)
            .css_classes(["flat", "circular"])
            .build();
        button.update_property(&[gtk::accessible::Property::Label(&format!(
            "{label} {}",
            item.name
        ))]);
        button.connect_clicked(move |button| run(button));
        buttons.append(&button);
    };
    let path = item.path.clone();
    make(
        "edit-copy-symbolic",
        "Copy",
        Box::new({
            let path = path.clone();
            move |button| copy_image(button, &path)
        }),
    );
    make(
        "document-open-symbolic",
        "Open",
        Box::new({
            let path = path.clone();
            move |button| open_image(button, &path)
        }),
    );
    make(
        "document-save-symbolic",
        "Save",
        Box::new({
            let (path, name) = (path.clone(), item.name.clone());
            move |button| match downloads.as_deref().map(|dir| save_to(dir, &path, &name)) {
                Some(Ok(saved)) => toast_from(
                    button,
                    &format!(
                        "Saved as {}",
                        saved.file_name().unwrap_or_default().to_string_lossy()
                    ),
                ),
                Some(Err(error)) => toast_from(button, &format!("Could not save it: {error}")),
                None => toast_from(button, "There is no Downloads folder"),
            }
        }),
    );
    make(
        "edit-delete-symbolic",
        "Remove",
        Box::new(move |button| {
            if gallery::remove(&path) {
                // The page redraws on the new generation.
                if let Some(window) = button.root().and_downcast::<DesklinkWindow>() {
                    window.refresh_soon();
                }
            }
        }),
    );
    tile.append(&buttons);
    tile
}

/// Reads and scales off the main thread, then paints (a picture gdk-pixbuf
/// cannot read, or one claiming too many pixels, stays blank).
fn load_thumbnail(picture: &gtk::Picture, path: PathBuf) {
    let weak = picture.downgrade();
    glib::spawn_future_local(async move {
        let decoded = gio::spawn_blocking(move || {
            if !crate::image_limits::file_within_bounds(
                &path,
                crate::image_limits::MAX_DECODE_PIXELS,
            ) {
                return None;
            }
            let pixbuf =
                gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(&path, THUMB_W * 2, THUMB_H * 2, true)
                    .ok()?;
            Some((
                pixbuf.width(),
                pixbuf.height(),
                pixbuf.rowstride(),
                pixbuf.has_alpha(),
                pixbuf.read_pixel_bytes(),
            ))
        })
        .await;
        let (Some(picture), Ok(Some((w, h, stride, alpha, bytes)))) = (weak.upgrade(), decoded)
        else {
            return;
        };
        let format = if alpha {
            gdk::MemoryFormat::R8g8b8a8
        } else {
            gdk::MemoryFormat::R8g8b8
        };
        picture.set_paintable(Some(&gdk::MemoryTexture::new(
            w,
            h,
            format,
            &bytes,
            stride as usize,
        )));
    });
}

/// The picture to the clipboard.
pub(super) fn copy_image(widget: &impl IsA<gtk::Widget>, path: &Path) {
    match gdk::Texture::from_filename(path) {
        Ok(texture) => {
            widget.clipboard().set_texture(&texture);
            toast_from(widget, "Copied");
        }
        Err(error) => toast_from(widget, &format!("Could not copy it: {error}")),
    }
}

fn open_image(widget: &impl IsA<gtk::Widget>, path: &Path) {
    let window = widget.root().and_downcast::<gtk::Window>();
    gtk::FileLauncher::new(Some(&gio::File::for_path(path))).launch(
        window.as_ref(),
        gio::Cancellable::NONE,
        |_| {},
    );
}

/// `name`, or `name (2)`, `name (3)`... in `dir`.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|path| !path.exists())
        .unwrap_or(first)
}

/// A copy into the Downloads folder; never overwrites.
fn save_to(dir: &Path, from: &Path, name: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let target = free_name(dir, name);
    std::fs::copy(from, &target).map_err(|error| error.to_string())?;
    Ok(target)
}

impl DesklinkWindow {
    /// A toast for each image that arrived since the last look.
    pub(super) fn show_gallery_arrivals(&self, devices: &[DeviceView]) {
        for arrived in gallery::take_arrived() {
            let phone = devices
                .iter()
                .find(|device| device.id == arrived.device_id)
                .map_or("your phone", |device| device.name.as_str());
            let toast = adw::Toast::new(&format!("Photo or screenshot from {phone}"));
            toast.set_use_markup(false);
            toast.set_timeout(8);
            toast.set_button_label(Some("Copy"));
            let (window, path) = (self.downgrade(), arrived.path);
            toast.connect_button_clicked(move |_| {
                if let Some(window) = window.upgrade() {
                    copy_image(&window, &path);
                }
            });
            self.queue_toast(&toast);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("desklink-gallery-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn saving_never_overwrites_and_keeps_the_extension() {
        let (from, downloads) = (dir(), dir());
        let image = from.join("x");
        std::fs::write(&image, b"png").unwrap();
        let first = save_to(&downloads, &image, "shot.png").unwrap();
        let second = save_to(&downloads, &image, "shot.png").unwrap();
        let third = save_to(&downloads, &image, "shot.png").unwrap();
        let names: Vec<_> = [first, second, third]
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["shot.png", "shot (2).png", "shot (3).png"]);
        assert_eq!(free_name(&downloads, "noext").file_name().unwrap(), "noext");
    }

    fn all_buttons(widget: &gtk::Widget, found: &mut Vec<gtk::Button>) {
        if let Some(button) = widget.downcast_ref::<gtk::Button>() {
            found.push(button.clone());
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            all_buttons(&next, found);
            child = next.next_sibling();
        }
    }

    /// The strip shows at most twelve tiles, only while photos are on and
    /// something is kept; Save puts a copy in Downloads and Remove deletes the
    /// kept file. (Needs a display.)
    #[test]
    fn the_strip_shows_the_latest_twelve_and_save_and_remove_work() {
        crate::test_display::run(|| {
            let info = crate::device_links::device_info::DeviceInfo::local(
                "gallery-ui-phone".into(),
                "Pixel".into(),
            );
            let device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
            let (src, downloads) = (dir(), dir());
            let root = gallery::root().unwrap();
            for n in 0..14u64 {
                let part = src.join(format!("p{n}"));
                std::fs::write(&part, b"not really a png").unwrap();
                gallery::store_in(&root, &device.id, &format!("s{n}.png"), &part, 1000 + n)
                    .unwrap();
            }
            let items: Vec<Item> = gallery::list_in(&root, &device.id)
                .into_iter()
                .take(STRIP_ITEMS)
                .collect();
            assert_eq!(items.len(), 12);
            assert_eq!(items[0].name, "s13.png", "newest first");
            assert!(build(&device, false, &items, None).is_none(), "off");
            assert!(build(&device, true, &[], None).is_none(), "nothing kept");
            let card = build(&device, true, &items, Some(downloads.clone())).unwrap();
            let mut buttons = Vec::new();
            all_buttons(card.upcast_ref(), &mut buttons);
            assert_eq!(buttons.len(), 12 * 4, "Copy, Open, Save, Remove each");
            let label_of = |b: &gtk::Button| b.tooltip_text().unwrap().to_string();
            let save = buttons.iter().find(|b| label_of(b) == "Save").unwrap();
            save.emit_clicked();
            assert!(downloads.join("s13.png").exists(), "a copy in Downloads");
            let remove = buttons.iter().find(|b| label_of(b) == "Remove").unwrap();
            remove.emit_clicked();
            assert!(!items[0].path.exists(), "the kept file is gone");
            assert_eq!(gallery::list_in(&root, &device.id).len(), 13);
            gallery::forget(&device.id);
        });
    }
}
