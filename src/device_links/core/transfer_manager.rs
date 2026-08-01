use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::errors::FeatureError;

pub const MAX_TRANSFER_SIZE: u64 = 4 * 1024 * 1024 * 1024;
pub const DEFAULT_TRANSFER_CHUNK_SIZE: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferState {
    Pending,
    Connecting,
    Transferring,
    Paused,
    Cancelling,
    Cancelled,
    Failed,
    Completed,
}

impl TransferState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Cancelled | Self::Failed | Self::Completed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferCheckpoint {
    pub transfer_id: String,
    pub device_id: String,
    pub filename: String,
    pub temporary_path: PathBuf,
    pub destination_path: PathBuf,
    #[serde(default)]
    pub source_path: Option<PathBuf>,
    pub total_size: u64,
    pub offset: u64,
    pub chunk_size: u64,
    pub sha256: Option<String>,
    pub resume_token: String,
    pub state: TransferState,
}

impl TransferCheckpoint {
    pub fn new(
        transfer_id: impl Into<String>,
        device_id: impl Into<String>,
        filename: impl Into<String>,
        temporary_path: PathBuf,
        destination_path: PathBuf,
        total_size: u64,
        resume_token: impl Into<String>,
    ) -> Result<Self, String> {
        if total_size > MAX_TRANSFER_SIZE {
            return Err("transfer exceeds the maximum supported size".to_string());
        }
        let transfer_id = transfer_id.into();
        if !is_safe_transfer_id(&transfer_id) {
            return Err("transfer id is invalid".to_string());
        }
        let resume_token = resume_token.into();
        if resume_token.is_empty() || resume_token.len() > 256 {
            return Err("resume token is invalid".to_string());
        }
        Ok(Self {
            transfer_id,
            device_id: device_id.into(),
            filename: filename.into(),
            temporary_path,
            destination_path,
            source_path: None,
            total_size,
            offset: 0,
            chunk_size: DEFAULT_TRANSFER_CHUNK_SIZE,
            sha256: None,
            resume_token,
            state: TransferState::Pending,
        })
    }

    pub fn advance(&mut self, next_offset: u64) -> Result<(), String> {
        if next_offset < self.offset {
            return Err("transfer offset cannot move backwards".to_string());
        }
        if next_offset > self.total_size {
            return Err("transfer offset exceeds the transfer size".to_string());
        }
        self.offset = next_offset;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.transfer_id.is_empty() || !is_safe_transfer_id(&self.transfer_id) {
            return Err("transfer id is invalid".to_string());
        }
        if self.device_id.is_empty() || self.filename.is_empty() {
            return Err("transfer identity is incomplete".to_string());
        }
        if self.total_size > MAX_TRANSFER_SIZE || self.offset > self.total_size {
            return Err("transfer checkpoint size or offset is invalid".to_string());
        }
        if self.chunk_size == 0 || self.chunk_size > 16 * 1024 * 1024 {
            return Err("transfer chunk size is invalid".to_string());
        }
        if self.resume_token.is_empty() || self.resume_token.len() > 256 {
            return Err("resume token is invalid".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferSnapshot {
    pub transfer_id: String,
    pub state: TransferState,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub can_resume: bool,
    pub retry_count: u32,
    pub error: Option<String>,
}

impl TransferSnapshot {
    pub fn from_checkpoint(checkpoint: &TransferCheckpoint, retry_count: u32) -> Self {
        Self {
            transfer_id: checkpoint.transfer_id.clone(),
            state: checkpoint.state,
            bytes_done: checkpoint.offset,
            bytes_total: checkpoint.total_size,
            // A failed or paused transfer is intentionally resumable.  Only a
            // completed or explicitly cancelled transfer must start over.
            can_resume: checkpoint.offset > 0
                && !matches!(
                    checkpoint.state,
                    TransferState::Completed | TransferState::Cancelled
                ),
            retry_count,
            error: None,
        }
    }
}

/// Durable storage for transfer checkpoints.
///
/// Checkpoints are deliberately stored separately from the identity/config
/// JSON. A partially completed transfer must never rewrite device identity,
/// certificates, or trusted-peer records.
#[derive(Debug, Clone)]
pub struct TransferCheckpointStore {
    directory: PathBuf,
}

impl TransferCheckpointStore {
    pub fn new(directory: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        Ok(Self { directory })
    }

    pub fn path_for(&self, transfer_id: &str) -> Result<PathBuf, String> {
        if !is_safe_transfer_id(transfer_id) {
            return Err("transfer id is invalid".to_string());
        }
        Ok(self.directory.join(format!("{transfer_id}.json")))
    }

    pub fn save(&self, checkpoint: &TransferCheckpoint) -> Result<(), String> {
        checkpoint.validate()?;
        let path = self.path_for(&checkpoint.transfer_id)?;
        let temporary = self.directory.join(format!(
            ".{}.tmp-{}",
            checkpoint.transfer_id,
            Uuid::new_v4()
        ));
        let bytes = serde_json::to_vec_pretty(checkpoint).map_err(|error| error.to_string())?;
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .map_err(|error| error.to_string())?;
            file.write_all(&bytes).map_err(|error| error.to_string())?;
            file.sync_all().map_err(|error| error.to_string())?;
            fs::rename(&temporary, &path).map_err(|error| error.to_string())?;
            sync_directory(&self.directory)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn load(&self, transfer_id: &str) -> Result<Option<TransferCheckpoint>, String> {
        let path = self.path_for(transfer_id)?;
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path).map_err(|error| error.to_string())?;
        let checkpoint: TransferCheckpoint =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        checkpoint.validate()?;
        Ok(Some(checkpoint))
    }

    pub fn remove(&self, transfer_id: &str) -> Result<(), String> {
        let path = self.path_for(transfer_id)?;
        match fs::remove_file(path) {
            Ok(()) => sync_directory(&self.directory),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn mark_cancelled(&self, transfer_id: &str) -> Result<Option<TransferSnapshot>, String> {
        let Some(mut checkpoint) = self.load(transfer_id)? else {
            return Ok(None);
        };
        checkpoint.state = TransferState::Cancelled;
        self.save(&checkpoint)?;
        Ok(Some(TransferSnapshot::from_checkpoint(&checkpoint, 0)))
    }
}

#[derive(Clone, Default)]
pub struct TransferManager {
    checkpoints: Arc<Mutex<HashMap<String, TransferCheckpoint>>>,
}

impl TransferManager {
    pub fn register(&self, checkpoint: TransferCheckpoint) -> Result<TransferSnapshot, String> {
        checkpoint.validate()?;
        let snapshot = TransferSnapshot::from_checkpoint(&checkpoint, 0);
        self.checkpoints
            .lock()
            .map_err(|_| "transfer manager lock poisoned".to_string())?
            .insert(checkpoint.transfer_id.clone(), checkpoint);
        Ok(snapshot)
    }

    pub fn update_offset(
        &self,
        transfer_id: &str,
        offset: u64,
    ) -> Result<TransferSnapshot, String> {
        let mut checkpoints = self
            .checkpoints
            .lock()
            .map_err(|_| "transfer manager lock poisoned".to_string())?;
        let checkpoint = checkpoints
            .get_mut(transfer_id)
            .ok_or_else(|| "transfer is not registered".to_string())?;
        checkpoint.advance(offset)?;
        checkpoint.state = if offset == checkpoint.total_size {
            TransferState::Completed
        } else {
            TransferState::Transferring
        };
        Ok(TransferSnapshot::from_checkpoint(checkpoint, 0))
    }

    pub fn set_state(
        &self,
        transfer_id: &str,
        state: TransferState,
    ) -> Result<TransferSnapshot, String> {
        let mut checkpoints = self
            .checkpoints
            .lock()
            .map_err(|_| "transfer manager lock poisoned".to_string())?;
        let checkpoint = checkpoints
            .get_mut(transfer_id)
            .ok_or_else(|| "transfer is not registered".to_string())?;
        checkpoint.state = state;
        Ok(TransferSnapshot::from_checkpoint(checkpoint, 0))
    }

    pub fn get(&self, transfer_id: &str) -> Option<TransferCheckpoint> {
        self.checkpoints.lock().ok()?.get(transfer_id).cloned()
    }
}

fn is_safe_transfer_id(transfer_id: &str) -> bool {
    !transfer_id.is_empty()
        && transfer_id.len() <= 128
        && transfer_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn sync_directory(directory: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let file = std::fs::File::open(directory).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn safe_destination(root: &Path, filename: &str) -> Result<PathBuf, FeatureError> {
    let path = Path::new(filename);
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(FeatureError::Invalid(
            "transfer filename is invalid".to_string(),
        ));
    };
    if name.is_empty() || name == "." || name == ".." || path.components().count() != 1 {
        return Err(FeatureError::Invalid(
            "transfer filename escapes its destination".to_string(),
        ));
    }
    let destination = root.join(name);
    if !destination.starts_with(root) {
        return Err(FeatureError::Invalid(
            "transfer destination is outside the download directory".to_string(),
        ));
    }
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::{
        safe_destination, TransferCheckpoint, TransferCheckpointStore, TransferManager,
        TransferState, MAX_TRANSFER_SIZE,
    };
    use std::fs;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn checkpoint(root: &std::path::Path) -> TransferCheckpoint {
        TransferCheckpoint::new(
            "transfer-1",
            "phone-1",
            "photo.jpg",
            root.join(".desklink-transfer-1.part"),
            root.join("photo.jpg"),
            10,
            "resume-token",
        )
        .unwrap()
    }

    #[test]
    fn safe_destination_accepts_a_single_normal_filename() {
        let root = PathBuf::from("/tmp/desklink-downloads");
        assert_eq!(
            safe_destination(&root, "photo.jpg").unwrap(),
            root.join("photo.jpg")
        );
    }

    #[test]
    fn checkpoint_offsets_are_monotonic_and_bounded() {
        let root = std::env::temp_dir().join(format!("desklink-transfer-{}", Uuid::new_v4()));
        let mut checkpoint = checkpoint(&root);
        assert!(checkpoint.advance(5).is_ok());
        assert_eq!(checkpoint.offset, 5);
        assert!(checkpoint.advance(4).is_err());
        assert!(checkpoint.advance(11).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn checkpoint_store_round_trips_atomically() {
        let root = std::env::temp_dir().join(format!("desklink-transfer-{}", Uuid::new_v4()));
        let store = TransferCheckpointStore::new(root.clone()).unwrap();
        let mut expected = checkpoint(&root);
        expected.advance(5).unwrap();
        expected.state = TransferState::Paused;
        store.save(&expected).unwrap();

        let actual = store.load("transfer-1").unwrap().unwrap();
        assert_eq!(actual, expected);
        assert!(store.path_for("../escape").is_err());
        store.remove("transfer-1").unwrap();
        assert!(store.load("transfer-1").unwrap().is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn manager_reports_progress_and_completion() {
        let manager = TransferManager::default();
        let root = std::env::temp_dir();
        manager.register(checkpoint(&root)).unwrap();
        let progress = manager.update_offset("transfer-1", 5).unwrap();
        assert_eq!(progress.bytes_done, 5);
        assert_eq!(progress.state, TransferState::Transferring);
        let complete = manager.update_offset("transfer-1", 10).unwrap();
        assert_eq!(complete.state, TransferState::Completed);
        assert!(!complete.can_resume);
    }

    #[test]
    fn cancellation_is_durable_and_not_resumable() {
        let root = std::env::temp_dir().join(format!("desklink-transfer-{}", Uuid::new_v4()));
        let store = TransferCheckpointStore::new(root.clone()).unwrap();
        let mut expected = checkpoint(&root);
        expected.advance(5).unwrap();
        expected.state = TransferState::Transferring;
        store.save(&expected).unwrap();

        let snapshot = store.mark_cancelled("transfer-1").unwrap().unwrap();
        assert_eq!(snapshot.state, TransferState::Cancelled);
        assert_eq!(snapshot.bytes_done, 5);
        assert!(!snapshot.can_resume);
        assert_eq!(
            store.load("transfer-1").unwrap().unwrap().state,
            TransferState::Cancelled
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_oversized_checkpoint() {
        let root = std::env::temp_dir();
        assert!(TransferCheckpoint::new(
            "transfer-1",
            "phone-1",
            "large.bin",
            root.join("part"),
            root.join("large.bin"),
            MAX_TRANSFER_SIZE + 1,
            "token",
        )
        .is_err());
    }
}
