use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionError {
    NotPaired,
    UnsupportedPacket(String),
    InvalidPacket(String),
    Timeout(&'static str),
    Transport(String),
    Authentication(String),
}

impl fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPaired => f.write_str("device is not paired"),
            Self::UnsupportedPacket(packet) => write!(f, "unsupported packet: {packet}"),
            Self::InvalidPacket(error) => write!(f, "invalid packet: {error}"),
            Self::Timeout(operation) => write!(f, "timed out during {operation}"),
            Self::Transport(error) => write!(f, "transport error: {error}"),
            Self::Authentication(error) => write!(f, "authentication error: {error}"),
        }
    }
}

impl std::error::Error for ConnectionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeatureError {
    Unsupported,
    Unauthorized,
    Invalid(String),
    BackendUnavailable(String),
    Failed(String),
}

impl fmt::Display for FeatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("feature is not supported"),
            Self::Unauthorized => f.write_str("feature is not authorized"),
            Self::Invalid(error) => write!(f, "invalid feature request: {error}"),
            Self::BackendUnavailable(error) => write!(f, "feature backend unavailable: {error}"),
            Self::Failed(error) => write!(f, "feature failed: {error}"),
        }
    }
}

impl std::error::Error for FeatureError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub accepted: bool,
    pub message: Option<String>,
}

impl CommandResult {
    pub fn accepted(message: impl Into<String>) -> Self {
        Self {
            accepted: true,
            message: Some(message.into()),
        }
    }

    pub fn rejected(message: impl Into<String>) -> Self {
        Self {
            accepted: false,
            message: Some(message.into()),
        }
    }
}
