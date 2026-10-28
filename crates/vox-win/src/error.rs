//! Windows errors.

/// A Win32/WinRT failure (message for logs and toasts).
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct WinError(pub String);

#[cfg(windows)]
impl From<windows::core::Error> for WinError {
    fn from(e: windows::core::Error) -> Self {
        WinError(e.message().to_string())
    }
}

impl From<std::io::Error> for WinError {
    fn from(e: std::io::Error) -> Self {
        WinError(e.to_string())
    }
}
