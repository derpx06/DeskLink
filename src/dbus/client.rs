use std::collections::HashMap;

use zbus::zvariant::OwnedValue;

use super::service::{BUS_NAME, INTERFACE, OBJECT_PATH};

pub type DeviceDetails = Vec<HashMap<String, OwnedValue>>;

pub struct DeskLinkClient {
    connection: zbus::blocking::Connection,
}

impl DeskLinkClient {
    pub fn connect() -> zbus::Result<Self> {
        Ok(Self {
            connection: zbus::blocking::Connection::session()?,
        })
    }

    fn proxy(&self) -> zbus::Result<zbus::blocking::Proxy<'_>> {
        zbus::blocking::Proxy::new(&self.connection, BUS_NAME, OBJECT_PATH, INTERFACE)
    }

    pub fn list_devices(&self) -> zbus::Result<DeviceDetails> {
        self.proxy()?.call("ListDevices", &())
    }

    pub fn pair(&self, device_id: &str) -> zbus::Result<bool> {
        self.proxy()?.call("Pair", &(device_id))
    }

    pub fn unpair(&self, device_id: &str) -> zbus::Result<bool> {
        self.proxy()?.call("Unpair", &(device_id))
    }

    pub fn ping(&self, device_id: &str) -> zbus::Result<bool> {
        self.proxy()?.call("Ping", &(device_id))
    }

    pub fn share_files(&self, device_id: &str, paths: &[String]) -> zbus::Result<String> {
        self.proxy()?.call("ShareFiles", &(device_id, paths))
    }

    pub fn share_url(&self, device_id: &str, url: &str) -> zbus::Result<bool> {
        self.proxy()?.call("ShareUrl", &(device_id, url))
    }

    pub fn set_clipboard(&self, device_id: &str, text: &str) -> zbus::Result<bool> {
        self.proxy()?.call("SetClipboard", &(device_id, text))
    }

    pub fn start_transfer(&self, device_id: &str, path: &str) -> zbus::Result<String> {
        self.proxy()?.call("StartTransfer", &(device_id, path))
    }

    pub fn cancel_transfer(&self, transfer_id: &str) -> zbus::Result<bool> {
        self.proxy()?.call("CancelTransfer", &(transfer_id))
    }

    pub fn get_transfer(&self, transfer_id: &str) -> zbus::Result<HashMap<String, OwnedValue>> {
        self.proxy()?.call("GetTransfer", &(transfer_id))
    }

    pub fn invoke_feature_action(
        &self,
        device_id: &str,
        action: &str,
        arguments: &HashMap<String, String>,
    ) -> zbus::Result<bool> {
        self.proxy()?
            .call("InvokeFeatureAction", &(device_id, action, arguments))
    }

    pub fn browse_sftp(&self, device_id: &str) -> zbus::Result<bool> {
        self.proxy()?.call("BrowseSftp", &(device_id))
    }

    pub fn get_preferences(&self) -> zbus::Result<HashMap<String, String>> {
        self.proxy()?.call("GetPreferences", &())
    }

    pub fn set_preference(&self, key: &str, value: &str) -> zbus::Result<bool> {
        self.proxy()?.call("SetPreference", &(key, value))
    }
}
