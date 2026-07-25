use std::fmt;

/// Driver-internal error carrying a message from the HTTP layer or a value
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
