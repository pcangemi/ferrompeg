use std::fmt;
use std::io;

/// Every fallible operation in the crate returns this error.
///
/// The variants separate "the file is broken" from "the file is fine but we
/// do not implement that feature" from "the user asked for something
/// impossible", because the CLI reports each of them differently.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The input does not follow the format it claims to be in.
    InvalidData(String),
    /// Valid input or a valid request that uses a feature we do not implement.
    Unsupported(String),
    /// Bad user input: a command line flag, a filter argument, a time value.
    InvalidArgument(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn invalid_data(msg: impl Into<String>) -> Self {
        Error::InvalidData(msg.into())
    }

    pub fn unsupported(msg: impl Into<String>) -> Self {
        Error::Unsupported(msg.into())
    }

    pub fn invalid_argument(msg: impl Into<String>) -> Self {
        Error::InvalidArgument(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                write!(f, "unexpected end of file")
            }
            Error::Io(e) => write!(f, "{e}"),
            Error::InvalidData(msg) => write!(f, "invalid data: {msg}"),
            Error::Unsupported(msg) => write!(f, "not supported: {msg}"),
            Error::InvalidArgument(msg) => write!(f, "invalid argument: {msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}
