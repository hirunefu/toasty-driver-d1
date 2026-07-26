//! Toasty driver for [Cloudflare D1](https://developers.cloudflare.com/d1/).
//!
//! D1 is SQLite, so SQL generation reuses toasty's SQLite serializer and
//! `Capability::SQLITE`; only the execution transport differs.
//!
//! # Transports
//!
//! - `rest` (default): D1's HTTP query API. Works from anywhere with an API
//!   token, and every operation is an HTTPS round trip to
//!   `/d1/database/{id}/raw`.
//! - `binding`: D1's Workers binding, via [`D1::from_binding`]. Only builds
//!   for `wasm32` and only runs inside a Worker, because the binding is a
//!   JavaScript object handed to the isolate rather than something a process
//!   can open. It is the faster of the two — a call inside the datacenter
//!   instead of a round trip out to the REST endpoint.
//!
//! # Caveats
//!
//! - Neither transport has interactive transactions: `Operation::Transaction`
//!   is rejected, and migrations apply statement-by-statement without a
//!   wrapping transaction. Writes that must commit together are submitted as
//!   one batch instead (`Capability::atomic_write_batch`).
//! - Integers beyond 2^53 lose precision in JSON transit (see `value`).
//! - `Bytes` columns are unsupported.
//!
//! # Examples
//!
//! ```ignore
//! // Anywhere, with an API token:
//! let driver = toasty_driver_d1::D1::from_env()?;
//!
//! // Inside a Worker, from the binding:
//! let driver = toasty_driver_d1::D1::from_binding(env.d1("DB")?);
//!
//! let db = toasty::Db::builder()
//!     .models(toasty::models!(crate::*))
//!     .build(driver)
//!     .await?;
//! ```

#[cfg(feature = "binding")]
mod binding;
mod error;
#[cfg(feature = "rest")]
mod http;
mod outcome;
mod transport;
mod value;

use std::{borrow::Cow, sync::Arc};

use async_trait::async_trait;
use toasty_core::{
    Result, Schema,
    driver::{
        Capability, ConnectContext, Driver, ExecResponse,
        operation::{Operation, RawSqlRet, TypedValue},
    },
    schema::{
        db::{AppliedMigration, Migration},
        diff,
    },
    stmt,
};
use toasty_sql as sql;

use crate::error::{D1Error, TransportError};
use crate::outcome::Want;
use crate::transport::Transport;

enum SqlReturn {
    Count,
    Infer,
    Types(Vec<stmt::Type>),
}

/// How this driver reaches the database.
enum Source {
    #[cfg(feature = "rest")]
    Rest {
        account_id: String,
        database_id: String,
        api_token: String,
        base_url: String,
    },
    /// A binding is already a live handle, so there is nothing to build per
    /// connection — unlike the HTTP transport, which is assembled from
    /// credentials each time.
    #[cfg(feature = "binding")]
    Binding(crate::binding::D1Binding),
}

/// A [`Driver`] that executes toasty operations against a Cloudflare D1
/// database, over either D1's HTTP API or its Workers binding.
pub struct D1 {
    source: Source,
}

impl D1 {
    /// Creates a driver for the given database, reached over the HTTP API.
    ///
    /// The token needs the `D1 Edit` permission for the account.
    #[cfg(feature = "rest")]
    pub fn new(
        account_id: impl Into<String>,
        database_id: impl Into<String>,
        api_token: impl Into<String>,
    ) -> Self {
        Self {
            source: Source::Rest {
                account_id: account_id.into(),
                database_id: database_id.into(),
                api_token: api_token.into(),
                base_url: http::DEFAULT_BASE_URL.to_string(),
            },
        }
    }

    /// Creates a driver from a D1 binding.
    ///
    /// This is the transport to prefer inside a Worker: the binding is a call
    /// within the datacenter, while the HTTP API is a round trip out to
    /// Cloudflare's REST endpoint and back.
    #[cfg(feature = "binding")]
    pub fn from_binding(database: worker::D1Database) -> Self {
        Self {
            source: Source::Binding(crate::binding::D1Binding::new(database)),
        }
    }

    /// Creates a driver from `D1_ACCOUNT_ID`, `D1_DATABASE_ID`, and
    /// `D1_API_TOKEN`.
    ///
    /// Deliberately not the `CLOUDFLARE_*` names: wrangler treats
    /// `CLOUDFLARE_API_TOKEN` as its own credential (including auto-loading
    /// it from `.env`), which hijacks OAuth logins in any repo that sets it.
    #[cfg(feature = "rest")]
    pub fn from_env() -> Result<Self> {
        let get = |key: &str| {
            std::env::var(key).map_err(|_| {
                toasty_core::Error::invalid_connection_url(format!("missing env var {key}"))
            })
        };
        Ok(Self::new(
            get("D1_ACCOUNT_ID")?,
            get("D1_DATABASE_ID")?,
            get("D1_API_TOKEN")?,
        ))
    }

    /// Overrides the API origin. Intended for tests against a mock server.
    #[cfg(feature = "rest")]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        // A match rather than `if let`: with only `rest` compiled in there is
        // a single variant and `if let` is irrefutable, but the second arm is
        // needed as soon as `binding` joins it.
        match &mut self.source {
            Source::Rest { base_url, .. } => *base_url = url.into(),
            #[cfg(feature = "binding")]
            Source::Binding(_) => {}
        }
        self
    }

    fn client(&self) -> Transport {
        match &self.source {
            #[cfg(feature = "rest")]
            Source::Rest {
                account_id,
                database_id,
                api_token,
                base_url,
            } => Transport::Rest(crate::http::D1Client::new(
                base_url,
                account_id,
                database_id,
                api_token,
            )),
            #[cfg(feature = "binding")]
            Source::Binding(binding) => Transport::Binding(binding.clone()),
        }
    }
}

impl std::fmt::Debug for D1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The API token must never reach logs.
        let mut out = f.debug_struct("D1");
        match &self.source {
            #[cfg(feature = "rest")]
            Source::Rest {
                account_id,
                database_id,
                ..
            } => {
                out.field("account_id", account_id)
                    .field("database_id", database_id);
            }
            #[cfg(feature = "binding")]
            Source::Binding(_) => {
                out.field("source", &"binding");
            }
        }
        out.finish_non_exhaustive()
    }
}

#[async_trait]
impl Driver for D1 {
    fn url(&self) -> Cow<'_, str> {
        match &self.source {
            #[cfg(feature = "rest")]
            Source::Rest {
                account_id,
                database_id,
                ..
            } => Cow::Owned(format!("d1://{account_id}/{database_id}")),
            // A binding names no account or database: the isolate is handed a
            // handle, not an address.
            #[cfg(feature = "binding")]
            Source::Binding(_) => Cow::Borrowed("d1://binding"),
        }
    }

    fn capability(&self) -> &'static Capability {
        // D1 is SQLite in every respect the planner cares about, except that
        // the HTTP API cannot open a transaction — so a read-only plan has to
        // be allowed to run without the snapshot one would otherwise give it.
        static CAPABILITY: std::sync::OnceLock<Capability> = std::sync::OnceLock::new();
        CAPABILITY.get_or_init(|| Capability {
            driver_name: "D1",
            // The HTTP API cannot open a transaction, so a read-only plan has
            // to run without the snapshot one would otherwise give it, and a
            // set of writes has to arrive together to commit together.
            snapshot_reads: false,
            atomic_write_batch: true,
            ..Capability::SQLITE
        })
    }

    async fn connect(&self, _cx: &ConnectContext) -> Result<Box<dyn toasty_core::Connection>> {
        Ok(Box::new(Connection {
            client: self.client(),
        }))
    }

    fn generate_migration(&self, schema_diff: &diff::Schema<'_>) -> Migration {
        let statements = sql::MigrationStatement::from_diff(schema_diff, &Capability::SQLITE);

        let sql_strings: Vec<String> = statements
            .iter()
            .map(|stmt| sql::Serializer::sqlite(stmt.schema()).serialize(stmt.statement()))
            .collect();

        Migration::new_sql_with_breakpoints(&sql_strings)
    }

    async fn reset_db(&self) -> Result<()> {
        // The HTTP API cannot drop-and-recreate the database itself without
        // changing its UUID, so drop every user table instead. `_cf_%` tables
        // are D1-internal.
        let client = self.client();
        let tables = client
            .raw(
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '_cf_%'",
                vec![],
                Want::Rows,
            )
            .await
            .map_err(into_core_error)?;

        for row in tables.rows {
            if let Some(serde_json::Value::String(name)) = row.first() {
                client
                    .raw(
                        &format!("DROP TABLE IF EXISTS \"{name}\""),
                        vec![],
                        Want::Changes,
                    )
                    .await
                    .map_err(into_core_error)?;
            }
        }

        Ok(())
    }
}

/// A "connection" to D1. Stateless: each exec is an independent HTTPS call.
#[derive(Debug)]
pub struct Connection {
    client: Transport,
}

/// Turns one statement's rows into the response shape the plan expects.
fn rows_to_response(outcome: crate::outcome::RawOutcome, ret: SqlReturn) -> Result<ExecResponse> {
    match ret {
        SqlReturn::Count => Ok(ExecResponse::count(outcome.changes)),
        SqlReturn::Infer => {
            let values = outcome
                .rows
                .iter()
                .map(|row| {
                    stmt::ValueRecord::from_vec(
                        row.iter().map(value::json_to_value_infer).collect(),
                    )
                    .into()
                })
                .collect();
            Ok(ExecResponse::value_stream(stmt::ValueStream::from_vec(
                values,
            )))
        }
        SqlReturn::Types(ret_tys) => {
            let mut values = vec![];
            for row in &outcome.rows {
                let items = ret_tys
                    .iter()
                    .enumerate()
                    .map(|(index, ty)| {
                        let cell = row.get(index).unwrap_or(&serde_json::Value::Null);
                        value::json_to_value(cell, ty)
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(toasty_core::Error::driver_operation_failed)?;
                values.push(stmt::ValueRecord::from_vec(items).into());
            }
            Ok(ExecResponse::value_stream(stmt::ValueStream::from_vec(
                values,
            )))
        }
    }
}

/// Substitutes `?N` placeholders with literals so several statements can share
/// one request, which is how D1 is given a set of writes to commit together.
fn inline_params(sql: &str, params: &[serde_json::Value]) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '?' {
            out.push(c);
            continue;
        }

        let mut digits = String::new();
        while let Some(d) = chars.peek().copied().filter(char::is_ascii_digit) {
            digits.push(d);
            chars.next();
        }

        match digits.parse::<usize>().ok().and_then(|n| params.get(n - 1)) {
            Some(value) => out.push_str(&sql_literal(value)),
            None => out.push_str("NULL"),
        }
    }

    out
}

fn sql_literal(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::Bool(b) => (if *b { "1" } else { "0" }).to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        // A blob literal; D1 returns these as arrays of byte values too.
        serde_json::Value::Array(items) => {
            let hex: String = items
                .iter()
                .filter_map(serde_json::Value::as_u64)
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("X'{hex}'")
        }
        other => format!("'{}'", other.to_string().replace('\'', "''")),
    }
}

fn into_core_error(err: TransportError) -> toasty_core::Error {
    match err {
        // A transport-level failure must classify as connection_lost so the
        // pool evicts the slot (see toasty's driver error contract).
        TransportError::Lost(e) => toasty_core::Error::connection_lost(e),
        TransportError::Api(e) => toasty_core::Error::driver_operation_failed(e),
    }
}

impl Connection {
    async fn exec_sql(
        &mut self,
        sql_str: &str,
        typed_params: Vec<TypedValue>,
        ret: SqlReturn,
    ) -> Result<ExecResponse> {
        let params = typed_params
            .iter()
            .map(|tv| value::param_to_json(&tv.value))
            .collect::<Result<Vec<_>, _>>()
            .map_err(toasty_core::Error::driver_operation_failed)?;

        // Which half of the outcome the plan will read, decided before the
        // statement runs because the binding transport cannot produce both.
        let want = match ret {
            SqlReturn::Count => Want::Changes,
            SqlReturn::Infer | SqlReturn::Types(_) => Want::Rows,
        };

        let outcome = self
            .client
            .raw(sql_str, params, want)
            .await
            .map_err(into_core_error)?;

        rows_to_response(outcome, ret)
    }
}

#[async_trait]
impl toasty_core::driver::Connection for Connection {
    async fn exec(&mut self, schema: &Arc<Schema>, op: Operation) -> Result<ExecResponse> {
        let (sql, typed_params, ret_tys) = match op {
            Operation::QuerySql(op) => {
                assert!(
                    op.last_insert_id_hack.is_none(),
                    "last_insert_id_hack is MySQL-specific and should not be set for D1"
                );
                (sql::Statement::from(op.stmt), op.params, op.ret)
            }
            Operation::RawSql(op) => {
                let ret = match op.ret {
                    RawSqlRet::None => SqlReturn::Count,
                    RawSqlRet::Infer => SqlReturn::Infer,
                    RawSqlRet::Types(types) => SqlReturn::Types(types),
                };
                return self.exec_sql(&op.sql, op.params, ret).await;
            }
            Operation::Transaction(_) => {
                return Err(toasty_core::Error::unsupported_feature(
                    "the D1 HTTP API does not support interactive transactions",
                ));
            }
            _ => {
                return Err(toasty_core::Error::unsupported_feature(format!(
                    "operation not supported by the D1 driver: {op:?}"
                )));
            }
        };

        let ret = match &sql {
            sql::Statement::Query(stmt) => match &stmt.body {
                stmt::ExprSet::Select(_) => SqlReturn::Types(ret_tys.unwrap()),
                _ => {
                    return Err(toasty_core::Error::unsupported_feature(
                        "non-SELECT query bodies are not supported by the D1 driver",
                    ));
                }
            },
            sql::Statement::Insert(stmt) => stmt
                .returning
                .as_ref()
                .map(|_| SqlReturn::Types(ret_tys.clone().unwrap()))
                .unwrap_or(SqlReturn::Count),
            sql::Statement::Delete(stmt) => stmt
                .returning
                .as_ref()
                .map(|_| SqlReturn::Types(ret_tys.clone().unwrap()))
                .unwrap_or(SqlReturn::Count),
            sql::Statement::Update(stmt) => {
                assert!(stmt.condition.is_none(), "stmt={stmt:#?}");
                stmt.returning
                    .as_ref()
                    .map(|_| SqlReturn::Types(ret_tys.clone().unwrap()))
                    .unwrap_or(SqlReturn::Count)
            }
            _ => SqlReturn::Count,
        };

        let sql_str = sql::Serializer::sqlite(&schema.db).serialize(&sql);
        self.exec_sql(&sql_str, typed_params, ret).await
    }

    async fn exec_batch(
        &mut self,
        schema: &Arc<Schema>,
        ops: Vec<Operation>,
    ) -> Result<Vec<ExecResponse>> {
        let mut sql = String::new();
        let mut returns = Vec::with_capacity(ops.len());

        for op in ops {
            let Operation::QuerySql(op) = op else {
                return Err(toasty_core::Error::unsupported_feature(
                    "the D1 driver batches SQL statements only",
                ));
            };

            let statement = sql::Statement::from(op.stmt);
            let ret = match &statement {
                sql::Statement::Insert(stmt) if stmt.returning.is_some() => {
                    SqlReturn::Types(op.ret.clone().expect("RETURNING declares its columns"))
                }
                sql::Statement::Update(stmt) if stmt.returning.is_some() => {
                    SqlReturn::Types(op.ret.clone().expect("RETURNING declares its columns"))
                }
                sql::Statement::Delete(stmt) if stmt.returning.is_some() => {
                    SqlReturn::Types(op.ret.clone().expect("RETURNING declares its columns"))
                }
                _ => SqlReturn::Count,
            };

            let text = sql::Serializer::sqlite(&schema.db).serialize(&statement);
            let params = op
                .params
                .iter()
                .map(|tv| value::param_to_json(&tv.value))
                .collect::<Result<Vec<_>, _>>()
                .map_err(toasty_core::Error::driver_operation_failed)?;

            // The serializer already terminates each statement; a second
            // semicolon would leave an empty one, which D1 rejects.
            let text = inline_params(&text, &params);
            sql.push_str(text.trim_end());
            if !text.trim_end().ends_with(';') {
                sql.push(';');
            }
            returns.push(ret);
        }

        let outcomes = self.client.raw_batch(&sql).await.map_err(into_core_error)?;

        if outcomes.len() != returns.len() {
            return Err(toasty_core::Error::driver_operation_failed(D1Error::new(
                format!(
                    "D1 returned {} results for {} statements",
                    outcomes.len(),
                    returns.len()
                ),
            )));
        }

        outcomes
            .into_iter()
            .zip(returns)
            .map(|(outcome, ret)| rows_to_response(outcome, ret))
            .collect()
    }

    async fn push_schema(&mut self, schema: &Schema) -> Result<()> {
        let serializer = sql::Serializer::sqlite(&schema.db);

        for table in &schema.db.tables {
            let stmt =
                serializer.serialize(&sql::Statement::create_table(table, &Capability::SQLITE));
            self.client
                .raw(&stmt, vec![], Want::Changes)
                .await
                .map_err(into_core_error)?;

            for index in &table.indices {
                if index.primary_key {
                    continue;
                }
                let stmt = serializer.serialize(&sql::Statement::create_index(index));
                self.client
                    .raw(&stmt, vec![], Want::Changes)
                    .await
                    .map_err(into_core_error)?;
            }
        }

        Ok(())
    }

    async fn applied_migrations(&mut self) -> Result<Vec<AppliedMigration>> {
        self.client
            .raw(
                "CREATE TABLE IF NOT EXISTS __toasty_migrations (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    applied_at TEXT NOT NULL
                )",
                vec![],
                Want::Changes,
            )
            .await
            .map_err(into_core_error)?;

        let outcome = self
            .client
            .raw(
                "SELECT id FROM __toasty_migrations ORDER BY applied_at",
                vec![],
                Want::Rows,
            )
            .await
            .map_err(into_core_error)?;

        outcome
            .rows
            .iter()
            .map(|row| {
                let id = row.first().and_then(|v| v.as_i64()).ok_or_else(|| {
                    toasty_core::Error::driver_operation_failed(D1Error::new(
                        "migration id is not an integer",
                    ))
                })?;
                Ok(AppliedMigration::new(id as u64))
            })
            .collect()
    }

    async fn apply_migration(&mut self, id: u64, name: &str, migration: &Migration) -> Result<()> {
        self.applied_migrations().await?;

        // Caveat: no wrapping transaction — the HTTP API cannot span one
        // across requests. A failed migration leaves earlier statements
        // applied; rerunning after a fix is the recovery path.
        for statement in migration.statements() {
            self.client
                .raw(statement, vec![], Want::Changes)
                .await
                .map_err(into_core_error)?;
        }

        self.client
            .raw(
                "INSERT INTO __toasty_migrations (id, name, applied_at) \
                 VALUES (?1, ?2, datetime('now'))",
                vec![serde_json::json!(id as i64), serde_json::json!(name)],
                Want::Changes,
            )
            .await
            .map_err(into_core_error)?;

        Ok(())
    }
}
