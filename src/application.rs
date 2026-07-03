/* application.rs
 *
 * Copyright 2026 manas
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

use adw::prelude::*;
use adw::subclass::prelude::*;
use gettextrs::gettext;
use gtk::{gio, glib};

use crate::config::VERSION;
use crate::device_links::daemon::DaemonHandle;
use crate::DesklinkWindow;

mod imp {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    pub struct DesklinkApplication {
        pub daemon: RefCell<Option<DaemonHandle>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DesklinkApplication {
        const NAME: &'static str = "DesklinkApplication";
        type Type = super::DesklinkApplication;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for DesklinkApplication {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            self.daemon.replace(Some(DaemonHandle::start()));
            obj.setup_gactions();
            obj.set_accels_for_action("app.quit", &["<control>q"]);
            obj.set_accels_for_action("app.shortcuts", &["<control>question"]);
        }
    }

    impl ApplicationImpl for DesklinkApplication {
        // We connect to the activate callback to create a window when the application
        // has been launched. Additionally, this callback notifies us when the user
        // tries to launch a "second instance" of the application. When they try
        // to do that, we'll just present any existing window.
        fn activate(&self) {
            let application = self.obj();
            let window = application.main_window();

            // Ask the window manager/compositor to present the window
            window.present();
        }
    }

    impl GtkApplicationImpl for DesklinkApplication {}
    impl AdwApplicationImpl for DesklinkApplication {}
}

glib::wrapper! {
    pub struct DesklinkApplication(ObjectSubclass<imp::DesklinkApplication>)
        @extends gio::Application, gtk::Application, adw::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl DesklinkApplication {
    pub fn new(application_id: &str, flags: &gio::ApplicationFlags) -> Self {
        glib::Object::builder()
            .property("application-id", application_id)
            .property("flags", flags)
            .property("resource-base-path", "/derx06/desklink/com")
            .build()
    }

    fn setup_gactions(&self) {
        let quit_action = gio::ActionEntry::builder("quit")
            .activate(move |app: &Self, _, _| app.quit())
            .build();
        let about_action = gio::ActionEntry::builder("about")
            .activate(move |app: &Self, _, _| app.show_about())
            .build();
        let preferences_action = gio::ActionEntry::builder("preferences")
            .activate(move |app: &Self, _, _| app.show_preferences())
            .build();
        let shortcuts_action = gio::ActionEntry::builder("shortcuts")
            .activate(move |app: &Self, _, _| app.show_shortcuts())
            .build();
        self.add_action_entries([
            quit_action,
            about_action,
            preferences_action,
            shortcuts_action,
        ]);
    }

    fn daemon_handle(&self) -> Option<DaemonHandle> {
        self.imp().daemon.borrow().clone()
    }

    fn main_window(&self) -> gtk::Window {
        self.active_window().unwrap_or_else(|| {
            let window = DesklinkWindow::new(self);
            if let Some(daemon) = self.daemon_handle() {
                window.set_daemon(daemon);
            }
            window.upcast()
        })
    }

    fn show_about(&self) {
        let window = self.main_window();
        let about = adw::AboutDialog::builder()
            .application_name("DeskLink")
            .application_icon("derx06.desklink.com")
            .developer_name("manas")
            .version(VERSION)
            .developers(vec!["manas"])
            // Translators: Replace "translator-credits" with your name/username, and optionally an email or URL.
            .translator_credits(gettext("translator-credits"))
            .copyright("© 2026 manas")
            .build();

        about.present(Some(&window));
    }

    fn show_preferences(&self) {
        let parent = self.main_window();
        let window = adw::Window::builder()
            .transient_for(&parent)
            .modal(true)
            .default_width(520)
            .default_height(360)
            .title("Preferences")
            .build();

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());

        let page = adw::PreferencesPage::new();
        let network_group = adw::PreferencesGroup::builder()
            .title("Connection")
            .description("DeskLink uses the KDE Connect LAN protocol on ports 1716-1764.")
            .build();

        let discovery_row = adw::ActionRow::builder()
            .title("Device Discovery")
            .subtitle("Broadcast identity packets and listen for nearby KDE Connect devices.")
            .build();
        let discover = gtk::Button::builder()
            .label("Discover Now")
            .valign(gtk::Align::Center)
            .css_classes(["suggested-action"])
            .build();
        if let Some(daemon) = self.daemon_handle() {
            discover.connect_clicked(move |_| {
                daemon.send(crate::device_links::daemon::DaemonCommand::Discover);
            });
        }
        discovery_row.add_suffix(&discover);
        network_group.add(&discovery_row);

        let clipboard_row = adw::ActionRow::builder()
            .title("Clipboard Sync")
            .subtitle("Enabled automatically for paired devices while DeskLink is running.")
            .build();
        clipboard_row.add_suffix(
            &gtk::Image::builder()
                .icon_name("emblem-ok-symbolic")
                .css_classes(["success"])
                .build(),
        );
        network_group.add(&clipboard_row);

        let downloads_row = adw::ActionRow::builder()
            .title("Received Files")
            .subtitle("Incoming files are saved to your Downloads folder with automatic renaming.")
            .build();
        network_group.add(&downloads_row);

        page.add(&network_group);
        toolbar.set_content(Some(&page));
        window.set_content(Some(&toolbar));
        window.present();
    }

    fn show_shortcuts(&self) {
        let parent = self.main_window();
        let window = adw::Window::builder()
            .transient_for(&parent)
            .modal(true)
            .default_width(420)
            .default_height(260)
            .title("Keyboard Shortcuts")
            .build();

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .margin_top(20)
            .margin_bottom(20)
            .margin_start(20)
            .margin_end(20)
            .css_classes(["boxed-list"])
            .build();

        for (title, shortcut) in [("Show Shortcuts", "Ctrl+?"), ("Quit", "Ctrl+Q")] {
            let row = adw::ActionRow::builder().title(title).build();
            row.add_suffix(
                &gtk::Label::builder()
                    .label(shortcut)
                    .valign(gtk::Align::Center)
                    .css_classes(["caption", "monospace"])
                    .build(),
            );
            list.append(&row);
        }

        toolbar.set_content(Some(&list));
        window.set_content(Some(&toolbar));
        window.present();
    }
}
