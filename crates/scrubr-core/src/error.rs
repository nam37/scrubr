//! One error type for the engine. Every error carries a short machine code, a message that says
//! what went wrong and, where there is one, a hint that says what to run next. The CLI turns the
//! kind into its exit code.

use std::fmt;
use std::io;

/// What went wrong, in the five groups the CLI reports as exit codes 1 to 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Bad usage: an unreadable time, a range outside the timeline, nothing to undo.
    Usage,
    /// The project or a file it names is missing.
    NotFound,
    /// The project changed since the version the command expected, or is locked by another writer.
    Conflict,
    /// A source file no longer matches the hash stored in the project.
    SourceChanged,
    /// A file could not be decoded, encoded, read or written.
    Codec,
}

impl Kind {
    pub fn exit_code(self) -> i32 {
        match self {
            Kind::Usage => 1,
            Kind::NotFound => 2,
            Kind::Conflict => 3,
            Kind::SourceChanged => 4,
            Kind::Codec => 5,
        }
    }
}

#[derive(Debug)]
pub struct Failure {
    pub kind: Kind,
    pub code: &'static str,
    pub message: String,
    pub hint: Option<String>,
}

#[derive(Debug)]
pub enum Error {
    Wav(hound::Error),
    Unsupported(String),
    Fail(Box<Failure>),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(kind: Kind, code: &'static str, message: impl Into<String>) -> Self {
        Error::Fail(Box::new(Failure { kind, code, message: message.into(), hint: None }))
    }

    pub fn usage(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(Kind::Usage, code, message)
    }

    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(Kind::NotFound, code, message)
    }

    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(Kind::Conflict, code, message)
    }

    pub fn codec(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(Kind::Codec, code, message)
    }

    /// Adds what to run next. Errors without a [`Failure`] are wrapped into one first.
    pub fn hint(self, hint: impl Into<String>) -> Self {
        let mut f = match self {
            Error::Fail(f) => f,
            other => Box::new(Failure { kind: other.kind(), code: other.code(), message: other.to_string(), hint: None }),
        };
        f.hint = Some(hint.into());
        Error::Fail(f)
    }

    pub fn kind(&self) -> Kind {
        match self {
            Error::Wav(hound::Error::IoError(e)) if e.kind() == io::ErrorKind::NotFound => Kind::NotFound,
            Error::Wav(_) | Error::Unsupported(_) => Kind::Codec,
            Error::Fail(f) => f.kind,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Error::Wav(hound::Error::IoError(e)) if e.kind() == io::ErrorKind::NotFound => "file_not_found",
            Error::Wav(_) => "decode_failed",
            Error::Unsupported(_) => "unsupported_audio",
            Error::Fail(f) => f.code,
        }
    }

    pub fn hint_text(&self) -> Option<&str> {
        match self {
            Error::Fail(f) => f.hint.as_deref(),
            _ => None,
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.kind().exit_code()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Wav(e) => write!(f, "could not read the WAV file: {e}"),
            Error::Unsupported(what) => write!(f, "unsupported audio: {what}"),
            Error::Fail(fail) => f.write_str(&fail.message),
        }
    }
}

impl std::error::Error for Error {}

impl From<hound::Error> for Error {
    fn from(e: hound::Error) -> Self {
        Error::Wav(e)
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::NotFound {
            Error::not_found("file_not_found", format!("File not found: {e}"))
        } else {
            Error::codec("io_error", format!("Read or write failed: {e}"))
        }
    }
}

/// An I/O error that names the file it happened on.
pub fn io_at(path: &std::path::Path, e: io::Error) -> Error {
    if e.kind() == io::ErrorKind::NotFound {
        Error::not_found("file_not_found", format!("{} does not exist.", path.display()))
    } else {
        Error::codec("io_error", format!("Could not read or write {}: {e}", path.display()))
    }
}
