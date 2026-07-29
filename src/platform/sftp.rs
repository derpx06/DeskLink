use gtk::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Debug)]
struct SftpSessionState {
    uri: String,
    cancellable: gio::Cancellable,
    mount: Option<gio::Mount>,
    closed: bool,
}

/// A GVfs-backed SFTP session owned by the window that opened it.
///
/// Keeping the mount and cancellable together is important: dropping the
/// async callback alone does not unmount a GVfs volume, and retaining a
/// password in DeskLink state would be unsafe. The session therefore stores
/// only the validated URI and the mount handle returned by GVfs.
#[derive(Clone)]
pub struct SftpSession {
    state: Rc<RefCell<SftpSessionState>>,
}

impl std::fmt::Debug for SftpSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SftpSession")
            .field("uri", &self.state.borrow().uri)
            .field("mounted", &self.state.borrow().mount.is_some())
            .field("closed", &self.state.borrow().closed)
            .finish()
    }
}

impl SftpSession {
    fn new(uri: &str) -> Self {
        Self {
            state: Rc::new(RefCell::new(SftpSessionState {
                uri: uri.to_string(),
                cancellable: gio::Cancellable::new(),
                mount: None,
                closed: false,
            })),
        }
    }

    pub fn uri(&self) -> String {
        self.state.borrow().uri.clone()
    }

    pub fn is_mounted(&self) -> bool {
        self.state.borrow().mount.is_some()
    }

    /// Cancel an in-flight mount and asynchronously unmount a completed GVfs
    /// mount. This method is intentionally idempotent so it can be called from
    /// both device-disconnect and window-close paths.
    pub fn close(&self) {
        let mount = {
            let mut state = self.state.borrow_mut();
            if state.closed {
                return;
            }
            state.closed = true;
            state.cancellable.cancel();
            state.mount.take()
        };

        if let Some(mount) = mount.filter(|mount| mount.can_unmount()) {
            mount.unmount_with_operation(
                gio::MountUnmountFlags::NONE,
                None::<&gio::MountOperation>,
                gio::Cancellable::NONE,
                |result| {
                    if let Err(error) = result {
                        glib::g_warning!("DeskLink", "SFTP unmount failed: {}", error);
                    }
                },
            );
        }
    }
}

/// Mount an Android SFTP endpoint through GVfs and hand it to the default
/// file manager. GtkMountOperation owns the credential prompt; DeskLink does
/// not retain the password in its device state.
pub fn mount_and_open_session(uri: &str, parent: &gtk::Window) -> Result<SftpSession, String> {
    let uri = uri.trim();
    validate_sftp_uri(uri)?;

    let file = gio::File::for_uri(uri);
    let operation = gtk::MountOperation::new(Some(parent));
    let session = SftpSession::new(uri);
    let session_for_callback = session.clone();
    let cancellable = session.state.borrow().cancellable.clone();
    let file_for_callback = file.clone();
    let cancellable_for_callback = cancellable.clone();
    let uri = uri.to_string();
    file.mount_enclosing_volume(
        gio::MountMountFlags::NONE,
        Some(&operation),
        Some(&cancellable),
        move |result| {
            let closed = session_for_callback.state.borrow().closed;
            match result {
                Ok(()) if !closed => {
                    match file_for_callback.find_enclosing_mount(Some(&cancellable_for_callback)) {
                        Ok(mount) => {
                            session_for_callback.state.borrow_mut().mount = Some(mount);
                            gio::AppInfo::launch_default_for_uri_async(
                                &uri,
                                gio::AppLaunchContext::NONE,
                                gio::Cancellable::NONE,
                                |result| {
                                    if let Err(error) = result {
                                        glib::g_warning!(
                                            "DeskLink",
                                            "Could not open the mounted SFTP location: {}",
                                            error
                                        );
                                    }
                                },
                            );
                        }
                        Err(error) => {
                            glib::g_warning!(
                                "DeskLink",
                                "Could not identify the mounted SFTP location: {}",
                                error
                            );
                        }
                    }
                }
                Ok(()) => {
                    // The user closed the window while mounting. Resolve the
                    // mount handle and immediately unmount it if GVfs raced
                    // the cancellation request.
                    if let Ok(mount) =
                        file_for_callback.find_enclosing_mount(Some(&cancellable_for_callback))
                    {
                        if mount.can_unmount() {
                            mount.unmount_with_operation(
                                gio::MountUnmountFlags::NONE,
                                None::<&gio::MountOperation>,
                                gio::Cancellable::NONE,
                                |_| {},
                            );
                        }
                    }
                }
                Err(error) if !closed => {
                    glib::g_warning!("DeskLink", "SFTP mount failed: {}", error);
                }
                Err(_) => {}
            }
        },
    );
    Ok(session)
}

/// Validate the authority and path before passing an Android-provided SFTP
/// location to GVfs.  `gio::File::for_uri` parses a URI but is not an access
/// policy; rejecting ambiguous authority/path forms here prevents a peer from
/// changing the destination or embedding credentials/control characters.
pub fn validate_sftp_uri(uri: &str) -> Result<(), String> {
    if !uri.starts_with("sftp://")
        || uri
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err("The remote file location is invalid".to_string());
    }
    let remainder = &uri[7..];
    let (authority, path) = remainder
        .split_once('/')
        .ok_or_else(|| "The SFTP location has no remote path".to_string())?;
    if authority.is_empty() || path.contains('?') || path.contains('#') {
        return Err("The remote SFTP authority or path is invalid".to_string());
    }
    let (user, host_port) = authority
        .rsplit_once('@')
        .ok_or_else(|| "The SFTP location has no user".to_string())?;
    if user.is_empty()
        || !user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err("The SFTP username is invalid".to_string());
    }
    validate_host_port(host_port)?;
    if path.split('/').any(|component| component == "..") {
        return Err("The SFTP path may not contain parent-directory traversal".to_string());
    }
    Ok(())
}

fn validate_host_port(host_port: &str) -> Result<(), String> {
    let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
        let (host, port) = rest
            .split_once(']')
            .ok_or_else(|| "The SFTP host is invalid".to_string())?;
        let port = port
            .strip_prefix(':')
            .ok_or_else(|| "The SFTP port is invalid".to_string())?;
        (host, port)
    } else {
        let (host, port) = host_port
            .rsplit_once(':')
            .ok_or_else(|| "The SFTP location has no port".to_string())?;
        (host, port)
    };
    if host.is_empty()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-_:".contains(&byte))
    {
        return Err("The SFTP host is invalid".to_string());
    }
    let port: u16 = port
        .parse()
        .map_err(|_| "The SFTP port is invalid".to_string())?;
    if port == 0 {
        return Err("The SFTP port is invalid".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_sftp_uri;

    #[test]
    fn accepts_normal_sftp_uri() {
        assert!(validate_sftp_uri("sftp://alice@192.0.2.10:1716/home/alice").is_ok());
        assert!(validate_sftp_uri("sftp://alice@[2001:db8::10]:1716/home/alice").is_ok());
    }

    #[test]
    fn rejects_ambiguous_authority_and_traversal() {
        assert!(validate_sftp_uri("sftp://alice@host@evil:22/home").is_err());
        assert!(validate_sftp_uri("sftp://alice@host:22/home/../etc").is_err());
        assert!(validate_sftp_uri("sftp://alice@host:0/home").is_err());
        assert!(validate_sftp_uri("sftp://alice@host:22/home name").is_err());
    }
}
