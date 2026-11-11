// SPDX-License-Identifier: GPL-3.0-or-later
//! Crate-wide error taxonomy.

/// The error type shared across trunkd crates.
///
/// Each variant is deliberately coarse; callers that need structured handling
/// match on the variant, while everything else can format the message.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An underlying I/O failure (file playback, socket, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Configuration could not be loaded or is semantically invalid.
    #[error("configuration error: {0}")]
    Config(String),

    /// A tuner/SDR device failed to open, tune, or stream.
    #[error("device error: {0}")]
    Device(String),

    /// A requested capability or format is not supported.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// Any error that does not fit a more specific variant.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Convenience constructor for [`Error::Config`].
    pub fn config(msg: impl Into<String>) -> Self {
        Error::Config(msg.into())
    }

    /// Convenience constructor for [`Error::Device`].
    pub fn device(msg: impl Into<String>) -> Self {
        Error::Device(msg.into())
    }

    /// Convenience constructor for [`Error::Unsupported`].
    pub fn unsupported(msg: impl Into<String>) -> Self {
        Error::Unsupported(msg.into())
    }
}

/// Standard result alias used throughout the workspace.
pub type Result<T> = std::result::Result<T, Error>;
