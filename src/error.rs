use std::fmt;

/// Driver-internal error carrying a message from a transport or a value
/// conversion; wrapped into `toasty_core::Error` at the trait boundary.
#[derive(Debug)]
pub(crate) struct D1Error {
    message: String,
}

impl D1Error {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for D1Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for D1Error {}

/// A failure from whichever transport is in use, split so the caller can
/// classify connection loss for the pool (see toasty's driver error contract).
///
/// Both variants carry a message rather than the underlying error: the two
/// transports have nothing in common to preserve — `reqwest::Error` is
/// `Send + Sync`, while the binding's errors wrap JavaScript values and are
/// neither.
#[derive(Debug)]
pub(crate) enum TransportError {
    /// The database could not be reached.
    Lost(D1Error),
    /// The database was reached and rejected the statement.
    Api(D1Error),
}
