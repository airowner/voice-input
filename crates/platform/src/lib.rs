//! Linux desktop adapter. Automatic delivery is disabled until the installation
//! has real capability evidence; failures always preserve the caller's text.
#[cfg(target_os = "linux")]
pub mod ipc;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod virtual_keyboard;
#[cfg(target_os = "linux")]
pub use linux::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("platform command unavailable or failed")]
    Command,
    #[error("platform operation timed out")]
    Timeout,
    #[error("platform output limit exceeded")]
    OutputLimit,
    #[error("desktop session unavailable")]
    Session,
    #[error("target cannot be safely identified")]
    Target,
    #[error("clipboard verification failed")]
    Clipboard,
    #[error("invalid control request")]
    InvalidRequest,
    #[error("private control socket unavailable")]
    Socket,
    #[error("another instance holds the session lock")]
    AlreadyRunning,
}
pub type Result<T> = std::result::Result<T, PlatformError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitySet {
    pub hyprland: bool,
    pub focus_events_connected: bool,
    pub automatic_delivery_verified: bool,
    pub clipboard_tools_present: bool,
    pub virtual_keyboard_tool_present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryOutcome {
    SentUnconfirmed,
    Manual,
    Unknown,
}
