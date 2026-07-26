//! What a statement produced, in the shape both transports report it.

/// One statement's outcome.
///
/// `rows` is positional rather than keyed by column name because that is what
/// the plan's return types are matched against, by index. A statement fills in
/// one field or the other, never both: a plan either counts changes or reads
/// rows (see `SqlReturn`).
#[derive(Debug, Default)]
pub(crate) struct RawOutcome {
    pub(crate) rows: Vec<Vec<serde_json::Value>>,
    pub(crate) changes: u64,
}

/// Which half of a [`RawOutcome`] the caller is going to read.
///
/// The HTTP API returns both for free, but D1's Workers binding splits them
/// across two calls (`raw` for rows, `run` for the change count), and running
/// the statement twice to fill in both would be wrong as well as wasteful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Want {
    Rows,
    Changes,
}
