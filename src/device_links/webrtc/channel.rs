use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelKind {
    Reliable,
    Realtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataChannelSpec {
    pub label: &'static str,
    pub ordered: bool,
    pub max_retransmits: Option<u16>,
    pub kind: ChannelKind,
}

impl DataChannelSpec {
    pub const CONTROL: Self = Self {
        label: "desklink-control-v1",
        ordered: true,
        max_retransmits: None,
        kind: ChannelKind::Reliable,
    };
    pub const EVENTS: Self = Self {
        label: "desklink-events-v1",
        ordered: true,
        max_retransmits: None,
        kind: ChannelKind::Reliable,
    };
    pub const INPUT_RELIABLE: Self = Self {
        label: "desklink-input-reliable-v1",
        ordered: true,
        max_retransmits: None,
        kind: ChannelKind::Reliable,
    };
    pub const INPUT_REALTIME: Self = Self {
        label: "desklink-input-realtime-v1",
        ordered: false,
        max_retransmits: Some(0),
        kind: ChannelKind::Realtime,
    };
    pub const FILE_CONTROL: Self = Self {
        label: "desklink-file-control-v1",
        ordered: true,
        max_retransmits: None,
        kind: ChannelKind::Reliable,
    };
    pub const FILE_DATA: Self = Self {
        label: "desklink-file-data-v1",
        ordered: true,
        max_retransmits: None,
        kind: ChannelKind::Reliable,
    };
    pub const TERMINAL: Self = Self {
        label: "desklink-terminal-v1",
        ordered: true,
        max_retransmits: None,
        kind: ChannelKind::Reliable,
    };

    pub const fn all() -> &'static [Self] {
        &[
            Self::CONTROL,
            Self::EVENTS,
            Self::INPUT_RELIABLE,
            Self::INPUT_REALTIME,
            Self::FILE_CONTROL,
            Self::FILE_DATA,
            Self::TERMINAL,
        ]
    }

    pub fn for_label(label: &str) -> Option<Self> {
        Self::all().iter().copied().find(|spec| spec.label == label)
    }

    pub fn validate_remote(
        label: &str,
        ordered: bool,
        max_retransmits: Option<u16>,
    ) -> Result<Self, ChannelError> {
        let spec =
            Self::for_label(label).ok_or_else(|| ChannelError::UnknownLabel(label.into()))?;
        if spec.ordered != ordered || spec.max_retransmits != max_retransmits {
            return Err(ChannelError::InvalidReliability(label.into()));
        }
        Ok(spec)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelError {
    UnknownLabel(String),
    InvalidReliability(String),
}

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownLabel(label) => write!(f, "unknown WebRTC data channel: {label}"),
            Self::InvalidReliability(label) => {
                write!(
                    f,
                    "invalid reliability settings for WebRTC data channel: {label}"
                )
            }
        }
    }
}

impl std::error::Error for ChannelError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_and_mismatched_channels() {
        assert!(matches!(
            DataChannelSpec::validate_remote("unknown", true, None),
            Err(ChannelError::UnknownLabel(_))
        ));
        assert!(matches!(
            DataChannelSpec::validate_remote("desklink-input-realtime-v1", true, Some(0)),
            Err(ChannelError::InvalidReliability(_))
        ));
        assert!(
            DataChannelSpec::validate_remote("desklink-input-realtime-v1", false, Some(0)).is_ok()
        );
    }
}
