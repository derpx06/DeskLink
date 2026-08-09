pub mod client;
mod gio_service;
mod service;

pub fn start(
    application: &gtk::gio::Application,
    handle: crate::device_links::daemon::DaemonHandle,
) {
    gio_service::start(application, handle);
}

pub fn start_headless(handle: crate::device_links::daemon::DaemonHandle) {
    service::start(handle);
}
