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

/// Splits a batch back into the statements it was joined from.
///
/// Needed only by the binding transport: `D1Database::batch` takes prepared
/// statements, while the HTTP API accepts the joined string as-is.
///
/// Splitting on `;` is safe for what the engine actually sends. Every
/// statement in a batch is a write built by toasty's SQL serializer, and its
/// parameters are already inlined -- as numbers, or quoted with `'` doubled.
/// So a `;` inside a string literal is the one case that would break this, and
/// it cannot occur without a parameter carrying one.
///
/// Caveat: that argument rests on the caller. A batch assembled by hand from
/// arbitrary SQL is outside what this handles.
// Only the binding transport calls this, but its tests are worth running on
// any target -- they pin a contract about what the engine sends, not about
// how it is delivered.
#[cfg_attr(not(feature = "binding"), allow(dead_code))]
pub(crate) fn split_statements(sql: &str) -> impl Iterator<Item = &str> {
    sql.split(';').map(str::trim).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::split_statements;

    fn split(sql: &str) -> Vec<&str> {
        split_statements(sql).collect()
    }

    #[test]
    fn splits_joined_statements_and_trims_them() {
        assert_eq!(
            split("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2);"),
            ["INSERT INTO t VALUES (1)", "INSERT INTO t VALUES (2)"]
        );
    }

    #[test]
    fn drops_the_empty_tail_after_a_trailing_semicolon() {
        // The serializer appends `;` to each statement, so a joined batch ends
        // with one. D1 rejects an empty statement outright, which is how this
        // was found in the first place.
        assert_eq!(split("UPDATE t SET a = 1;"), ["UPDATE t SET a = 1"]);
        assert_eq!(split("UPDATE t SET a = 1;;  ;"), ["UPDATE t SET a = 1"]);
    }

    #[test]
    fn a_lone_statement_without_a_semicolon_survives() {
        assert_eq!(split("DELETE FROM t"), ["DELETE FROM t"]);
    }

    #[test]
    fn nothing_at_all_yields_nothing() {
        assert!(split("").is_empty());
        assert!(split("   ;  ; ").is_empty());
    }
}
