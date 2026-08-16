//! One error type for the whole crate.
//!
//! Every variant carries the thing a person needs to act: the path, the
//! encoding that had no decoder, the feature that was not compiled in. A
//! message that says only "decode failed" costs more time than it saves.

use std::path::PathBuf;

use crate::identify::Encoding;

/// What went wrong.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The file, or the call, was about something not implemented here.
    ///
    /// Carries what was asked for and, where there is one, the way to get it.
    #[error("{what}")]
    Unsupported {
        /// What cannot be done, and what to do instead.
        what: String,
    },

    /// The bytes are a container this crate cannot read.
    #[error("{path}: not a container vtome reads ({found})")]
    UnknownContainer {
        /// The file.
        path: PathBuf,
        /// What identification made of it.
        found: String,
    },

    /// Nothing in this build can decode that encoding on this machine.
    ///
    /// Names the feature or the platform backend that would have handled it,
    /// because "no decoder" on its own leaves a caller guessing between a
    /// missing cargo feature and a missing operating system.
    #[error("no decoder for {encoding}: {remedy}")]
    NoDecoder {
        /// The encoding that went unhandled.
        encoding: Encoding,
        /// The feature to enable, or the platform that would have done it.
        remedy: String,
    },

    /// The container is malformed, or ends where it should not.
    #[error("{path}: {reason}")]
    Demux {
        /// The file.
        path: PathBuf,
        /// What did not add up.
        reason: String,
    },

    /// A decoder rejected a packet.
    #[error("decoding {encoding}: {reason}")]
    Decode {
        /// What was being decoded.
        encoding: Encoding,
        /// What the decoder said.
        reason: String,
    },

    /// An encoder failed.
    #[error("encoding: {reason}")]
    Encode {
        /// What the encoder said.
        reason: String,
    },

    /// A frame's planes do not describe a picture of that size.
    #[error("{reason}")]
    BadFrame {
        /// Which dimension, stride, or plane count disagreed.
        reason: String,
    },

    /// No monitor answers that selector.
    #[error("no monitor matches {selector} ({available} attached)")]
    NoSuchMonitor {
        /// The selector as it was written.
        selector: String,
        /// How many monitors were actually there.
        available: usize,
    },

    /// The placement cannot be drawn: a quad that folds over itself, a
    /// rectangle with no area, an opacity outside 0..=1.
    #[error("{reason}")]
    Placement {
        /// What is wrong with it.
        reason: String,
    },

    /// The GPU could not be set up, or a surface was lost.
    #[error("render: {reason}")]
    Render {
        /// What the graphics layer said.
        reason: String,
    },

    /// [`std::io::Error`] alone does not say which file it was about, and by
    /// the time it reaches a user that is the only part they need.
    #[error("{path}: {source}")]
    Io {
        /// The file or directory the call was about.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    /// Attaches a path to an [`std::io::Error`].
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    /// Something this build, or this platform, cannot do.
    pub(crate) fn unsupported(what: impl Into<String>) -> Self {
        Error::Unsupported { what: what.into() }
    }

    /// A container that does not parse.
    #[cfg_attr(not(feature = "demux"), allow(dead_code))]
    pub(crate) fn demux(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        Error::Demux {
            path: path.into(),
            reason: reason.into(),
        }
    }

    /// A placement that cannot be drawn.
    pub(crate) fn placement(reason: impl Into<String>) -> Self {
        Error::Placement {
            reason: reason.into(),
        }
    }
}

/// [`Result`](std::result::Result) with this crate's error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
