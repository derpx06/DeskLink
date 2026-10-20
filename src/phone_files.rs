//! The "Phone Files" browser: navigate the folders the phone has chosen to
//! share, download files, and manage them, over the authenticated WebRTC file
//! channel.
//!
//! [`BrowserModel`] holds every decision about what a reply *means* and has no
//! GTK in it, so it is unit-tested. The rest of the file only draws it.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::device_links::daemon::DaemonHandle;
use crate::device_links::webrtc::file_browser::{
    self, BrowserEntry, BrowserRequest, BrowserResponse, BrowserResult,
};

/// How often replies are collected. Short enough that a click feels answered,
/// long enough to cost nothing.
const POLL_INTERVAL: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Crumb {
    entry_id: String,
    name: String,
}

/// What the user asked for, remembered until the phone answers.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Intent {
    Roots,
    Open(Crumb),
    /// Listing the parent folder; the trail is popped when it succeeds.
    Up,
    Reload,
    /// The next page of the folder being shown; its entries are appended.
    More,
    /// A change to the phone's files; the message is shown once it succeeds and
    /// the listing is then refreshed.
    Change(String),
    Download(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Notice {
    Info(String),
    Error(String),
}

/// What the view must do after a reply.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// A reply to nothing we are waiting for.
    Ignored,
    Redraw,
    /// A change succeeded: re-list the folder so the view shows the result.
    Reload,
}

#[derive(Debug, Default)]
struct BrowserModel {
    /// Empty means the list of storage roots.
    trail: Vec<Crumb>,
    entries: Vec<BrowserEntry>,
    /// Where the phone said the rest of this folder starts; `None` when what is
    /// shown is all of it.
    next_offset: Option<u32>,
    in_flight: Option<(String, Intent)>,
    notice: Option<Notice>,
}

impl BrowserModel {
    fn busy(&self) -> bool {
        self.in_flight.is_some()
    }

    fn folder_id(&self) -> Option<&str> {
        self.trail.last().map(|crumb| crumb.entry_id.as_str())
    }

    /// The folder to list when going up, or `None` when that means the roots.
    fn parent_id(&self) -> Option<&str> {
        let len = self.trail.len();
        (len >= 2).then(|| self.trail[len - 2].entry_id.as_str())
    }

    fn title(&self) -> String {
        self.trail
            .last()
            .map(|crumb| crumb.name.clone())
            .unwrap_or_else(|| "Phone files".to_string())
    }

    fn path_text(&self) -> String {
        self.trail
            .iter()
            .map(|crumb| crumb.name.as_str())
            .collect::<Vec<_>>()
            .join(" › ")
    }

    /// A fresh listing replaces the old one, and with it the way to continue it.
    fn show(&mut self, entries: &[BrowserEntry], next_offset: Option<u32>) {
        self.entries = entries.to_vec();
        self.next_offset = next_offset;
    }

    fn begin(&mut self, request_id: String, intent: Intent) {
        self.in_flight = Some((request_id, intent));
        self.notice = None;
    }

    /// The re-listing that follows a successful change. It keeps that change's
    /// message ("Deleted …") on screen instead of wiping it as a new request
    /// normally would.
    fn begin_refresh(&mut self, request_id: String) {
        self.in_flight = Some((request_id, Intent::Reload));
    }

    fn fail(&mut self, error: String) {
        self.notice = Some(Notice::Error(error));
    }

    fn apply(&mut self, response: &BrowserResponse) -> Step {
        let matches = self
            .in_flight
            .as_ref()
            .is_some_and(|(id, _)| *id == response.request_id);
        if !matches {
            return Step::Ignored;
        }
        let (_, intent) = self.in_flight.take().expect("checked above");
        let result = match &response.result {
            Ok(result) => result,
            Err(error) => {
                self.notice = Some(Notice::Error(error.clone()));
                return Step::Redraw;
            }
        };
        match (intent, result) {
            (
                Intent::Roots,
                BrowserResult::Entries {
                    entries,
                    next_offset,
                    ..
                },
            ) => {
                self.trail.clear();
                self.show(entries, *next_offset);
                if entries.is_empty() {
                    self.notice = Some(Notice::Info(
                        "The phone is not sharing any folders yet. On the phone, choose \
                         folders to share in DeskLink's file settings."
                            .to_string(),
                    ));
                }
            }
            (
                Intent::Open(crumb),
                BrowserResult::Entries {
                    entries,
                    next_offset,
                    ..
                },
            ) => {
                self.trail.push(crumb);
                self.show(entries, *next_offset);
            }
            (
                Intent::Up,
                BrowserResult::Entries {
                    entries,
                    next_offset,
                    ..
                },
            ) => {
                self.trail.pop();
                self.show(entries, *next_offset);
            }
            (
                Intent::Reload,
                BrowserResult::Entries {
                    entries,
                    next_offset,
                    ..
                },
            ) => self.show(entries, *next_offset),
            (
                Intent::More,
                BrowserResult::Entries {
                    entries,
                    next_offset,
                    ..
                },
            ) => {
                self.entries.extend(entries.iter().cloned());
                self.next_offset = *next_offset;
            }
            (Intent::Change(message), _) => {
                self.notice = Some(Notice::Info(message));
                return Step::Reload;
            }
            (Intent::Download(name), BrowserResult::Download { .. }) => {
                self.notice = Some(Notice::Info(format!(
                    "Receiving “{name}”. It will appear in your Downloads folder."
                )));
            }
            _ => {
                self.notice = Some(Notice::Error(
                    "The phone sent an unexpected reply".to_string(),
                ));
            }
        }
        Step::Redraw
    }
}

struct Ui {
    window: adw::Window,
    title: adw::WindowTitle,
    list: gtk::ListBox,
    /// A message over the listing; when there is no listing it is shown as
    /// the whole page instead.
    banner: adw::Banner,
    stack: gtk::Stack,
    status_page: adw::StatusPage,
    retry: gtk::Button,
    spinner: gtk::Spinner,
    up: gtk::Button,
    refresh: gtk::Button,
    new_folder: gtk::Button,
}

struct Ctx {
    daemon: DaemonHandle,
    device_id: String,
    model: RefCell<BrowserModel>,
    ui: Ui,
    closed: Cell<bool>,
}

pub fn show(daemon: DaemonHandle, device_id: String, device_name: &str, parent: &gtk::Window) {
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(640)
        .default_height(600)
        .title(format!("{device_name} files"))
        .build();
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("Phone files", "");
    header.set_title_widget(Some(&title));
    let up = icon_button("go-up-symbolic", "Up One Folder");
    let refresh = icon_button("view-refresh-symbolic", "Refresh");
    let new_folder = icon_button("folder-new-symbolic", "New Folder");
    header.pack_start(&up);
    header.pack_end(&new_folder);
    header.pack_end(&refresh);
    let spinner = gtk::Spinner::builder().spinning(true).build();
    header.pack_end(&spinner);
    toolbar.add_top_bar(&header);
    let banner = adw::Banner::builder().use_markup(false).build();
    toolbar.add_top_bar(&banner);

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .valign(gtk::Align::Start)
        .css_classes(["boxed-list"])
        .build();
    let retry = gtk::Button::builder()
        .label("Try Again")
        .halign(gtk::Align::Center)
        .css_classes(["pill"])
        .build();
    let status_page = adw::StatusPage::builder().child(&retry).build();
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    stack.add_named(
        &gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(
                &adw::Clamp::builder()
                    .margin_top(12)
                    .margin_bottom(24)
                    .margin_start(12)
                    .margin_end(12)
                    .child(&list)
                    .build(),
            )
            .build(),
        Some("list"),
    );
    stack.add_named(&status_page, Some("status"));
    stack.add_named(
        &gtk::Spinner::builder()
            .spinning(true)
            .width_request(48)
            .height_request(48)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build(),
        Some("loading"),
    );
    toolbar.set_content(Some(&stack));
    window.set_content(Some(&toolbar));

    let ctx = Rc::new(Ctx {
        daemon,
        device_id,
        model: RefCell::new(BrowserModel::default()),
        ui: Ui {
            window: window.clone(),
            title,
            list,
            banner,
            stack,
            status_page,
            retry: retry.clone(),
            spinner,
            up: up.clone(),
            refresh: refresh.clone(),
            new_folder: new_folder.clone(),
        },
        closed: Cell::new(false),
    });

    // Every widget handler holds a `Weak`, so the window and the context never
    // keep each other alive. The timer below is the one strong owner and ends
    // when the window closes.
    let weak = Rc::downgrade(&ctx);
    up.connect_clicked({
        let weak = weak.clone();
        move |_| with_ctx(&weak, go_up)
    });
    refresh.connect_clicked({
        let weak = weak.clone();
        move |_| with_ctx(&weak, reload)
    });
    retry.connect_clicked({
        let weak = weak.clone();
        move |_| with_ctx(&weak, reload)
    });
    new_folder.connect_clicked({
        let weak = weak.clone();
        move |_| with_ctx(&weak, prompt_new_folder)
    });
    window.connect_close_request({
        let weak = weak.clone();
        move |_| {
            if let Some(ctx) = weak.upgrade() {
                ctx.closed.set(true);
            }
            glib::Propagation::Proceed
        }
    });

    // Anything left over from an earlier dialog answers a question nobody is
    // asking any more.
    let _ = ctx.daemon.take_phone_file_replies(&ctx.device_id);
    request(&ctx, BrowserRequest::Roots, Intent::Roots);
    render(&ctx);

    glib::timeout_add_local(POLL_INTERVAL, {
        let ctx = Rc::clone(&ctx);
        move || {
            if ctx.closed.get() {
                return glib::ControlFlow::Break;
            }
            poll(&ctx);
            glib::ControlFlow::Continue
        }
    });
    window.present();
}

fn with_ctx(weak: &Weak<Ctx>, action: impl FnOnce(&Rc<Ctx>)) {
    if let Some(ctx) = weak.upgrade() {
        action(&ctx);
    }
}

fn icon_button(icon: &str, label: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(label)
        .build();
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    button
}

/// Sends `request` unless one is already waiting; the UI keeps one in flight so
/// a reply can only ever answer the question the model remembers.
fn request(ctx: &Rc<Ctx>, request: BrowserRequest, intent: Intent) {
    if ctx.model.borrow().busy() {
        return;
    }
    match ctx.daemon.browse_phone(&ctx.device_id, &request) {
        Ok(request_id) => ctx.model.borrow_mut().begin(request_id, intent),
        Err(error) => ctx.model.borrow_mut().fail(error),
    }
    render(ctx);
}

fn poll(ctx: &Rc<Ctx>) {
    let mut reload = false;
    let mut redraw = false;
    for response in ctx.daemon.take_phone_file_replies(&ctx.device_id) {
        match ctx.model.borrow_mut().apply(&response) {
            Step::Ignored => {}
            Step::Redraw => redraw = true,
            Step::Reload => {
                redraw = true;
                reload = true;
            }
        }
    }
    if reload {
        refresh_after_change(ctx);
    } else if redraw {
        render(ctx);
    }
}

/// Re-lists the current folder after a successful change, keeping the change's
/// message on screen.
fn refresh_after_change(ctx: &Rc<Ctx>) {
    let Some(entry_id) = ctx.model.borrow().folder_id().map(str::to_string) else {
        return reload(ctx);
    };
    match ctx.daemon.browse_phone(
        &ctx.device_id,
        &BrowserRequest::List {
            entry_id,
            offset: 0,
        },
    ) {
        Ok(request_id) => ctx.model.borrow_mut().begin_refresh(request_id),
        Err(error) => ctx.model.borrow_mut().fail(error),
    }
    render(ctx);
}

fn reload(ctx: &Rc<Ctx>) {
    let folder = ctx.model.borrow().folder_id().map(str::to_string);
    match folder {
        Some(entry_id) => request(
            ctx,
            BrowserRequest::List {
                entry_id,
                offset: 0,
            },
            Intent::Reload,
        ),
        None => request(ctx, BrowserRequest::Roots, Intent::Roots),
    }
}

fn load_more(ctx: &Rc<Ctx>) {
    let (folder, offset) = {
        let model = ctx.model.borrow();
        (model.folder_id().map(str::to_string), model.next_offset)
    };
    if let (Some(entry_id), Some(offset)) = (folder, offset) {
        request(ctx, BrowserRequest::List { entry_id, offset }, Intent::More);
    }
}

fn go_up(ctx: &Rc<Ctx>) {
    let parent = ctx.model.borrow().parent_id().map(str::to_string);
    match parent {
        Some(entry_id) => request(
            ctx,
            BrowserRequest::List {
                entry_id,
                offset: 0,
            },
            Intent::Up,
        ),
        None => request(ctx, BrowserRequest::Roots, Intent::Roots),
    }
}

fn open_folder(ctx: &Rc<Ctx>, entry: &BrowserEntry) {
    request(
        ctx,
        BrowserRequest::List {
            entry_id: entry.entry_id.clone(),
            offset: 0,
        },
        Intent::Open(Crumb {
            entry_id: entry.entry_id.clone(),
            name: entry.name.clone(),
        }),
    );
}

fn download(ctx: &Rc<Ctx>, entry: &BrowserEntry) {
    request(
        ctx,
        BrowserRequest::Download {
            entry_id: entry.entry_id.clone(),
            transfer_id: file_browser::new_transfer_id(),
        },
        Intent::Download(entry.name.clone()),
    );
}

fn prompt_new_folder(ctx: &Rc<Ctx>) {
    let Some(parent_id) = ctx.model.borrow().folder_id().map(str::to_string) else {
        return; // Storage roots cannot hold new folders.
    };
    prompt_name(ctx, "New folder", "Create", "", move |ctx, name| {
        request(
            ctx,
            BrowserRequest::CreateFolder {
                parent_id: parent_id.clone(),
                name: name.clone(),
            },
            Intent::Change(format!("Created “{}”", name.trim())),
        );
    });
}

fn prompt_rename(ctx: &Rc<Ctx>, entry: &BrowserEntry) {
    let entry_id = entry.entry_id.clone();
    prompt_name(ctx, "Rename", "Rename", &entry.name, move |ctx, name| {
        request(
            ctx,
            BrowserRequest::Rename {
                entry_id: entry_id.clone(),
                name: name.clone(),
            },
            Intent::Change(format!("Renamed to “{}”", name.trim())),
        );
    });
}

fn prompt_delete(ctx: &Rc<Ctx>, entry: &BrowserEntry) {
    let body = if entry.directory {
        format!("“{}” and everything inside it will be permanently deleted from your phone. This cannot be undone.", entry.name)
    } else {
        format!(
            "“{}” will be permanently deleted from your phone. This cannot be undone.",
            entry.name
        )
    };
    let dialog = adw::AlertDialog::new(Some("Delete from phone?"), Some(&body));
    dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let weak = Rc::downgrade(ctx);
    let entry_id = entry.entry_id.clone();
    let name = entry.name.clone();
    dialog.choose(&ctx.ui.window, gio::Cancellable::NONE, move |response| {
        if response == "delete" {
            with_ctx(&weak, |ctx| {
                request(
                    ctx,
                    BrowserRequest::Delete {
                        entry_id: entry_id.clone(),
                    },
                    Intent::Change(format!("Deleted “{name}”")),
                );
            });
        }
    });
}

/// Asks for a file or folder name and runs `done` with it. The name is
/// validated by [`BrowserRequest`] before anything is sent, so a bad one comes
/// back as a message rather than a failed round trip.
fn prompt_name(
    ctx: &Rc<Ctx>,
    heading: &str,
    confirm: &str,
    initial: &str,
    done: impl Fn(&Rc<Ctx>, String) + 'static,
) {
    let entry = gtk::Entry::builder()
        .text(initial)
        .activates_default(true)
        .max_length(file_browser::MAX_NAME_LEN as i32)
        .build();
    entry.update_property(&[gtk::accessible::Property::Label("Name")]);
    let dialog = adw::AlertDialog::new(Some(heading), None);
    dialog.add_responses(&[("cancel", "Cancel"), ("ok", confirm)]);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    dialog.set_extra_child(Some(&entry));
    let weak = Rc::downgrade(ctx);
    dialog.choose(&ctx.ui.window, gio::Cancellable::NONE, move |response| {
        if response == "ok" {
            let name = entry.text().to_string();
            with_ctx(&weak, |ctx| done(ctx, name));
        }
    });
}

fn render(ctx: &Rc<Ctx>) {
    let model = ctx.model.borrow();
    let ui = &ctx.ui;
    let busy = model.busy();
    let in_folder = !model.trail.is_empty();
    let listing = !model.entries.is_empty();

    ui.title.set_title(&model.title());
    ui.title.set_subtitle(&model.path_text());
    ui.spinner.set_visible(busy && listing);
    ui.up.set_sensitive(!busy && in_folder);
    ui.refresh.set_sensitive(!busy);
    ui.new_folder.set_sensitive(!busy && in_folder);

    let message = match &model.notice {
        Some(Notice::Info(message) | Notice::Error(message)) => Some(message.as_str()),
        None => None,
    };
    ui.banner.set_title(message.unwrap_or_default());
    ui.banner.set_revealed(listing && message.is_some());

    if !listing {
        if busy {
            ui.stack.set_visible_child_name("loading");
        } else {
            let failed = matches!(model.notice, Some(Notice::Error(_)));
            let (icon, title) = match (failed, in_folder) {
                (true, _) => ("dialog-warning-symbolic", "Could Not Load Files"),
                (false, true) => ("folder-symbolic", "Folder Is Empty"),
                (false, false) => ("folder-remote-symbolic", "No Shared Folders"),
            };
            ui.status_page.set_icon_name(Some(icon));
            ui.status_page.set_title(title);
            ui.status_page.set_description(
                message
                    .map(|message| glib::markup_escape_text(message).to_string())
                    .as_deref(),
            );
            ui.retry.set_visible(failed);
            ui.stack.set_visible_child_name("status");
        }
    } else {
        ui.stack.set_visible_child_name("list");
    }

    ui.list.remove_all();
    let weak = Rc::downgrade(ctx);
    for entry in &model.entries {
        ui.list.append(&entry_row(&weak, entry, busy));
    }
    if model.next_offset.is_some() {
        let more = adw::ActionRow::builder()
            .title("Show More")
            .activatable(true)
            .sensitive(!busy)
            .build();
        more.add_suffix(&gtk::Image::from_icon_name("go-down-symbolic"));
        more.connect_activated({
            let weak = weak.clone();
            move |_| with_ctx(&weak, load_more)
        });
        ui.list.append(&more);
    }
}

fn entry_row(weak: &Weak<Ctx>, entry: &BrowserEntry, busy: bool) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&entry.name))
        .subtitle(glib::markup_escape_text(&entry_subtitle(entry)))
        .build();
    let icon = if entry.directory {
        gio::ThemedIcon::new("folder-symbolic").upcast::<gio::Icon>()
    } else {
        let (content_type, _) = gio::content_type_guess(Some(&entry.name), &[]);
        gio::content_type_get_symbolic_icon(&content_type)
    };
    row.add_prefix(&gtk::Image::builder().gicon(&icon).pixel_size(24).build());

    if entry.directory && entry.readable {
        row.set_activatable(!busy);
        let weak = weak.clone();
        let entry = entry.clone();
        row.connect_activated(move |_| with_ctx(&weak, |ctx| open_folder(ctx, &entry)));
    } else if !entry.directory && entry.readable {
        let button = icon_button("folder-download-symbolic", "Download");
        button.set_valign(gtk::Align::Center);
        button.set_sensitive(!busy);
        button.add_css_class("flat");
        let weak = weak.clone();
        let entry = entry.clone();
        button.connect_clicked(move |_| with_ctx(&weak, |ctx| download(ctx, &entry)));
        row.add_suffix(&button);
    }

    // Only entries inside a shared folder can be changed; roots are the
    // user's own choice of what to share.
    if entry.writable && entry.parent_id.is_some() {
        row.add_suffix(&entry_menu(weak, entry, busy));
    }
    if entry.directory && entry.readable {
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    }
    row
}

fn entry_menu(weak: &Weak<Ctx>, entry: &BrowserEntry, busy: bool) -> gtk::MenuButton {
    let popover = gtk::Popover::new();
    popover.add_css_class("menu");
    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    let rename = gtk::Button::builder()
        .label("Rename…")
        .css_classes(["flat"])
        .build();
    let delete = gtk::Button::builder()
        .label("Delete…")
        .css_classes(["flat", "error"])
        .build();
    {
        let weak = weak.clone();
        let entry = entry.clone();
        let popover = popover.clone();
        rename.connect_clicked(move |_| {
            popover.popdown();
            with_ctx(&weak, |ctx| prompt_rename(ctx, &entry));
        });
    }
    {
        let weak = weak.clone();
        let entry = entry.clone();
        let popover = popover.clone();
        delete.connect_clicked(move |_| {
            popover.popdown();
            with_ctx(&weak, |ctx| prompt_delete(ctx, &entry));
        });
    }
    actions.append(&rename);
    actions.append(&delete);
    popover.set_child(Some(&actions));
    let menu = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("More actions")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .popover(&popover)
        .sensitive(!busy)
        .build();
    menu.update_property(&[gtk::accessible::Property::Label("More actions")]);
    menu
}

fn entry_subtitle(entry: &BrowserEntry) -> String {
    let mut parts = Vec::new();
    if !entry.directory {
        parts.push(glib::format_size(entry.size).to_string());
    }
    if entry.modified > 0 {
        if let Ok(date) = glib::DateTime::from_unix_local((entry.modified / 1000) as i64)
            .and_then(|date| date.format("%x"))
        {
            parts.push(date.to_string());
        }
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str, directory: bool) -> BrowserEntry {
        BrowserEntry {
            entry_id: id.into(),
            root_id: "root".into(),
            parent_id: Some("root".into()),
            name: name.into(),
            directory,
            size: 10,
            modified: 0,
            mime_type: None,
            readable: true,
            writable: true,
        }
    }

    fn listing(id: &str, entries: Vec<BrowserEntry>) -> BrowserResponse {
        BrowserResponse {
            request_id: id.into(),
            result: Ok(BrowserResult::Entries {
                parent_id: None,
                entries,
                next_offset: None,
            }),
        }
    }

    fn partial(id: &str, entries: Vec<BrowserEntry>, next_offset: u32) -> BrowserResponse {
        let mut response = listing(id, entries);
        if let Ok(BrowserResult::Entries {
            next_offset: next, ..
        }) = &mut response.result
        {
            *next = Some(next_offset);
        }
        response
    }

    fn crumb(id: &str, name: &str) -> Crumb {
        Crumb {
            entry_id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn opening_and_leaving_folders_walks_the_trail() {
        let mut model = BrowserModel::default();
        model.begin("1".into(), Intent::Roots);
        assert_eq!(
            model.apply(&listing("1", vec![entry("r", "Internal", true)])),
            Step::Redraw
        );
        assert!(model.trail.is_empty());

        model.begin("2".into(), Intent::Open(crumb("r", "Internal")));
        model.apply(&listing("2", vec![entry("d", "DCIM", true)]));
        model.begin("3".into(), Intent::Open(crumb("d", "DCIM")));
        model.apply(&listing("3", vec![entry("f", "a.jpg", false)]));

        assert_eq!(model.path_text(), "Internal › DCIM");
        assert_eq!(model.title(), "DCIM");
        assert_eq!(model.folder_id(), Some("d"));
        assert_eq!(model.parent_id(), Some("r"));

        model.begin("4".into(), Intent::Up);
        model.apply(&listing("4", vec![entry("d", "DCIM", true)]));
        assert_eq!(model.path_text(), "Internal");
        assert_eq!(
            model.parent_id(),
            None,
            "the parent of a root is the roots list"
        );
    }

    #[test]
    fn a_folder_larger_than_one_reply_is_continued_page_by_page() {
        let mut model = BrowserModel::default();
        model.begin("1".into(), Intent::Open(crumb("d", "DCIM")));
        model.apply(&partial("1", vec![entry("a", "a", false)], 1));
        assert_eq!(model.next_offset, Some(1));

        model.begin("2".into(), Intent::More);
        assert_eq!(
            model.apply(&partial("2", vec![entry("b", "b", false)], 2)),
            Step::Redraw
        );
        assert_eq!(
            model.entries.len(),
            2,
            "a later page is appended, not swapped in"
        );
        assert_eq!(model.next_offset, Some(2));

        model.begin("3".into(), Intent::More);
        model.apply(&listing("3", vec![entry("c", "c", false)]));
        let names: Vec<_> = model.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert_eq!(model.next_offset, None, "the last page ends the paging");
        assert_eq!(model.path_text(), "DCIM", "paging never moves the trail");
    }

    #[test]
    fn a_failed_page_keeps_the_listing_and_the_way_to_retry() {
        let mut model = BrowserModel::default();
        model.begin("1".into(), Intent::Open(crumb("d", "DCIM")));
        model.apply(&partial("1", vec![entry("a", "a", false)], 1));

        model.begin("2".into(), Intent::More);
        model.apply(&BrowserResponse {
            request_id: "2".into(),
            result: Err("The phone did not continue the listing".into()),
        });

        assert_eq!(model.entries.len(), 1);
        assert_eq!(model.next_offset, Some(1));
        assert!(matches!(model.notice, Some(Notice::Error(_))));
    }

    #[test]
    fn another_listing_forgets_the_previous_folders_continuation() {
        let mut model = BrowserModel::default();
        model.begin("1".into(), Intent::Open(crumb("d", "DCIM")));
        model.apply(&partial("1", vec![entry("a", "a", false)], 1));

        model.begin("2".into(), Intent::Reload);
        model.apply(&listing("2", vec![entry("a", "a", false)]));
        assert_eq!(model.next_offset, None);

        model.apply(&partial("x", vec![], 9)); // not awaited: ignored
        assert_eq!(model.next_offset, None);

        model.begin("3".into(), Intent::Reload);
        model.apply(&partial("3", vec![entry("a", "a", false)], 1));
        model.begin("4".into(), Intent::Roots);
        model.apply(&listing("4", vec![entry("r", "Internal", true)]));
        assert_eq!(model.next_offset, None, "the roots list is never paged");
    }

    #[test]
    fn a_reply_to_something_else_changes_nothing() {
        let mut model = BrowserModel::default();
        model.begin("1".into(), Intent::Open(crumb("r", "Internal")));

        assert_eq!(model.apply(&listing("stale", vec![])), Step::Ignored);
        assert!(model.busy(), "the real reply is still awaited");
        assert!(model.trail.is_empty());

        let mut idle = BrowserModel::default();
        assert_eq!(idle.apply(&listing("1", vec![])), Step::Ignored);
    }

    #[test]
    fn a_failure_keeps_the_current_folder_and_shows_the_reason() {
        let mut model = BrowserModel::default();
        model.begin("1".into(), Intent::Open(crumb("r", "Internal")));
        model.apply(&listing("1", vec![entry("f", "a.txt", false)]));

        model.begin("2".into(), Intent::Open(crumb("x", "Locked")));
        let step = model.apply(&BrowserResponse {
            request_id: "2".into(),
            result: Err("Entry is not a readable folder".into()),
        });

        assert_eq!(step, Step::Redraw);
        assert_eq!(model.path_text(), "Internal");
        assert_eq!(model.entries.len(), 1);
        assert_eq!(
            model.notice,
            Some(Notice::Error("Entry is not a readable folder".into()))
        );
        assert!(!model.busy());
    }

    #[test]
    fn a_successful_change_asks_for_a_fresh_listing() {
        let mut model = BrowserModel::default();
        model.begin("1".into(), Intent::Change("Deleted “a.txt”".into()));

        let step = model.apply(&BrowserResponse {
            request_id: "1".into(),
            result: Ok(BrowserResult::Deleted {
                entry_id: "a".into(),
            }),
        });

        assert_eq!(step, Step::Reload);
        assert_eq!(model.notice, Some(Notice::Info("Deleted “a.txt”".into())));
    }

    #[test]
    fn the_refresh_after_a_change_keeps_the_success_message() {
        let mut model = BrowserModel::default();
        model.begin("1".into(), Intent::Change("Deleted “a.txt”".into()));
        model.apply(&BrowserResponse {
            request_id: "1".into(),
            result: Ok(BrowserResult::Deleted {
                entry_id: "a".into(),
            }),
        });

        model.begin_refresh("2".into());
        assert_eq!(model.notice, Some(Notice::Info("Deleted “a.txt”".into())));

        model.apply(&listing("2", vec![]));
        assert_eq!(
            model.notice,
            Some(Notice::Info("Deleted “a.txt”".into())),
            "the fresh listing must not erase it either"
        );

        // A user-requested request starts clean.
        model.begin("3".into(), Intent::Reload);
        assert_eq!(model.notice, None);
    }

    #[test]
    fn an_empty_roots_list_explains_itself() {
        let mut model = BrowserModel::default();
        model.begin("1".into(), Intent::Roots);
        model.apply(&listing("1", vec![]));
        assert!(matches!(model.notice, Some(Notice::Info(_))));
    }

    #[test]
    fn a_reply_of_the_wrong_shape_is_an_error_not_a_listing() {
        let mut model = BrowserModel::default();
        model.begin("1".into(), Intent::Open(crumb("r", "Internal")));
        model.apply(&BrowserResponse {
            request_id: "1".into(),
            result: Ok(BrowserResult::Deleted {
                entry_id: "x".into(),
            }),
        });
        assert!(model.trail.is_empty(), "the trail must not advance");
        assert!(matches!(model.notice, Some(Notice::Error(_))));
    }

    #[test]
    fn a_download_reply_reports_where_the_file_will_land() {
        let mut model = BrowserModel::default();
        model.begin("1".into(), Intent::Download("a.zip".into()));
        model.apply(&BrowserResponse {
            request_id: "1".into(),
            result: Ok(BrowserResult::Download {
                transfer_id: "t".into(),
                name: "a.zip".into(),
                size: 5,
            }),
        });
        let Some(Notice::Info(message)) = &model.notice else {
            panic!("expected an info notice");
        };
        assert!(message.contains("a.zip") && message.contains("Downloads"));
    }
}
