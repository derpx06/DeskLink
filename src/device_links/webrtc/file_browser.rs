use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PHONE_FILE_BROWSER_MESSAGE_TYPE: &str = "desklink.file.browser.v1";
pub const PHONE_FILE_BROWSER_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PhoneFileAction {
    Roots,
    List,
    Metadata,
    CreateFolder,
    Rename,
    Move,
    Delete,
    Download,
    Upload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneFileRequest {
    pub browser_version: u8,
    pub request_id: String,
    pub action: PhoneFileAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneFileResponse {
    pub browser_version: u8,
    pub request_id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl PhoneFileRequest {
    pub fn roots(request_id: String) -> Self {
        Self {
            browser_version: PHONE_FILE_BROWSER_VERSION,
            request_id,
            action: PhoneFileAction::Roots,
            entry_id: None,
            destination_id: None,
            name: None,
            transfer_id: None,
        }
    }

    pub fn list(request_id: String, entry_id: String) -> Self {
        Self {
            browser_version: PHONE_FILE_BROWSER_VERSION,
            request_id,
            action: PhoneFileAction::List,
            entry_id: Some(entry_id),
            destination_id: None,
            name: None,
            transfer_id: None,
        }
    }

    pub fn download(request_id: String, entry_id: String, transfer_id: String) -> Self {
        Self {
            browser_version: PHONE_FILE_BROWSER_VERSION,
            request_id,
            action: PhoneFileAction::Download,
            entry_id: Some(entry_id),
            destination_id: None,
            name: None,
            transfer_id: Some(transfer_id),
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.browser_version != PHONE_FILE_BROWSER_VERSION {
            return Err("unsupported phone-file protocol version");
        }
        if self.request_id.is_empty() || self.request_id.len() > 128 {
            return Err("invalid phone-file request ID");
        }
        if self.entry_id.as_ref().is_some_and(|id| id.len() > 128)
            || self
                .destination_id
                .as_ref()
                .is_some_and(|id| id.len() > 128)
        {
            return Err("invalid phone-file entry ID");
        }
        if let Some(transfer_id) = &self.transfer_id {
            if transfer_id.is_empty()
                || transfer_id.len() > 128
                || !transfer_id.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                })
            {
                return Err("invalid phone-file transfer ID");
            }
        }
        if matches!(
            self.action,
            PhoneFileAction::List
                | PhoneFileAction::Metadata
                | PhoneFileAction::Rename
                | PhoneFileAction::Move
                | PhoneFileAction::Delete
                | PhoneFileAction::Download
        ) && self.entry_id.as_deref().is_none_or(str::is_empty)
        {
            return Err("phone-file operation requires an entry ID");
        }
        if self.action == PhoneFileAction::Download && self.transfer_id.is_none() {
            return Err("download requires a transfer ID");
        }
        if let Some(name) = &self.name {
            if name.is_empty()
                || name.len() > 255
                || matches!(name.as_str(), "." | "..")
                || name.contains(['/', '\\', '\0'])
            {
                return Err("invalid phone-file name");
            }
        }
        Ok(())
    }
}

impl PhoneFileResponse {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.browser_version != PHONE_FILE_BROWSER_VERSION
            || self.request_id.is_empty()
            || self.request_id.len() > 128
            || (self.ok && self.result.is_none())
            || (!self.ok && self.error.as_deref().is_none_or(str::is_empty))
        {
            return Err("malformed phone-file response");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_schema_matches_android_and_rejects_traversal_names() {
        let request = PhoneFileRequest::roots("request-1".to_string());
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"browserVersion":1,"requestId":"request-1","action":"roots"}"#
        );
        let mut rename = request;
        rename.action = PhoneFileAction::Rename;
        rename.entry_id = Some("opaque-entry".to_string());
        rename.name = Some("../escape".to_string());
        assert!(rename.validate().is_err());

        let download = PhoneFileRequest::download(
            "request-2".to_string(),
            "opaque-entry".to_string(),
            "transfer-1".to_string(),
        );
        download.validate().unwrap();
        assert_eq!(download.action, PhoneFileAction::Download);
    }

    #[test]
    fn response_requires_result_or_error_matching_status() {
        let response = PhoneFileResponse {
            browser_version: 1,
            request_id: "request-1".to_string(),
            ok: true,
            result: Some(serde_json::json!({"entries": []})),
            error: None,
        };
        response.validate().unwrap();
        let malformed = PhoneFileResponse {
            result: None,
            ..response
        };
        assert!(malformed.validate().is_err());
    }
}
