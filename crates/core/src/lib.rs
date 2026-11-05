pub mod config;
pub mod controller;
pub mod recovery;

pub use config::*;
pub use controller::*;
pub use recovery::*;
use serde::{Deserialize, Serialize};

/// Boundary-safe errors contain no native diagnostic, text, path or credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct AppError {
    pub code: String,
    pub stage: String,
    pub retryable: bool,
    pub message: String,
}

impl AppError {
    pub fn new(code: &str, stage: &str, retryable: bool, message: &str) -> Self {
        Self {
            code: code.into(),
            stage: stage.into(),
            retryable,
            message: message.into(),
        }
    }
}

pub(crate) fn invalid_config(message: &str) -> AppError {
    AppError::new("invalid_config", "config", false, message)
}
