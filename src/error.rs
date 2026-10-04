use std::fmt;

/// Errors returned by overhook.
#[derive(Debug)]
pub enum Error {
    /// A Windows / Direct3D call failed.
    Windows(windows::core::Error),
    /// Creating or enabling a function hook failed.
    Hook(String),
    /// [`crate::OverlayBuilder::install`] was called while an overlay is installed.
    AlreadyInstalled,
    /// No UI backend was configured on the builder.
    NoBackend,
    /// No supported graphics API could be hooked.
    NoGraphicsApi,
    /// Shader compilation failed (message from the HLSL compiler).
    Shader(String),
    /// Any other error.
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Windows(e) => write!(f, "windows error: {e}"),
            Error::Hook(e) => write!(f, "hook error: {e}"),
            Error::AlreadyInstalled => f.write_str("the overlay is already installed"),
            Error::NoBackend => f.write_str("no UI backend configured"),
            Error::NoGraphicsApi => f.write_str("no supported graphics API could be hooked"),
            Error::Shader(e) => write!(f, "shader compilation failed: {e}"),
            Error::Other(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Windows(e) => Some(e),
            _ => None,
        }
    }
}

impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Error::Windows(e)
    }
}

/// Result alias used inside the crate.
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;
