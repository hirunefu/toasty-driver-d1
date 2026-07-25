//! Toasty driver for [Cloudflare D1](https://developers.cloudflare.com/d1/)
//! over its HTTP query API.
//!
//! D1 is SQLite, so SQL generation reuses toasty's SQLite serializer and
//! `Capability::SQLITE`; only the execution transport differs — every
//! operation is an HTTPS round trip to `/d1/database/{id}/raw`.
//!
//! # Caveats
//!
//! - Every query pays a network round trip; latency is HTTP-API latency, not
//!   local-SQLite latency.
//! - The HTTP API has no interactive transactions: `Operation::Transaction`
//!   is rejected, and migrations apply statement-by-statement without a
//!   wrapping transaction. Anything Toasty builds on transactions — a
//!   multi-record `create!`, an eager load — fails with it.
//! - Integers beyond ±2^53 are rejected when binding, because D1's JSON
//!   transport would otherwise corrupt them silently (see `value`).
//!
//! # Examples
//!
//! ```ignore
//! let driver = toasty_driver_d1::D1::from_env()?;
//! let db = toasty::Db::builder()
//!     .models(toasty::models!(crate::*))
//!     .build(driver)
//!     .await?;
//! ```

mod error;
mod http;
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

use crate::error::D1Error;
use crate::http::{D1Client, HttpError};

enum SqlReturn {
    Count,
    Infer,
    Types(Vec<stmt::Type>),
}

/// A [`Driver`] that executes toasty operations against a Cloudflare D1
/// database through the HTTP API.
pub struct D1 {
    account_id: String,
    database_id: String,
    api_token: String,
    base_url: String,
}

impl D1 {
    /// Creates a driver for the given database.
    ///
    /// The token needs the `D1 Edit` permission for the account.
    pub fn new(
        account_id: impl Into<String>,
        database_id: impl Into<String>,
        api_token: impl Into<String>,
    ) -> Self {
        Self {
            account_id: account_id.into(),
            database_id: database_id.into(),
            api_token: api_token.into(),
            base_url: http::DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Creates a driver from `D1_ACCOUNT_ID`, `D1_DATABASE_ID`, and
    /// `D1_API_TOKEN`.
    ///
    /// Deliberately not the `CLOUDFLARE_*` names: wrangler treats
    /// `CLOUDFLARE_API_TOKEN` as its own credential (including auto-loading
    /// it from `.env`), which hijacks OAuth logins in any repo that sets it.
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
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    fn client(&self) -> D1Client {
        D1Client::new(
            &self.base_url,
            &self.account_id,
            &self.database_id,
            &self.api_token,
        )
    }
}

impl std::fmt::Debug for D1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The API token must never reach logs.
        f.debug_struct("D1")
            .field("account_id", &self.account_id)
            .field("database_id", &self.database_id)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Driver for D1 {
    fn url(&self) -> Cow<'_, str> {
        Cow::Owned(format!("d1://{}/{}", self.account_id, self.database_id))
    }

    fn capability(&self) -> &'static Capability {
        &Capability::SQLITE
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
            )
            .await
            .map_err(into_core_error)?;

        for row in tables.rows {
            if let Some(serde_json::Value::String(name)) = row.first() {
                client
                    .raw(&format!("DROP TABLE IF EXISTS \"{name}\""), vec![])
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
    client: D1Client,
}

fn into_core_error(err: HttpError) -> toasty_core::Error {
    match err {
        // A transport-level failure must classify as connection_lost so the
        // pool evicts the slot (see toasty's driver error contract).
        HttpError::Transport(e) => toasty_core::Error::connection_lost(e),
        HttpError::Api(e) => toasty_core::Error::driver_operation_failed(e),
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

        let outcome = self
            .client
            .raw(sql_str, params)
            .await
            .map_err(into_core_error)?;

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

    async fn push_schema(&mut self, schema: &Schema) -> Result<()> {
        let serializer = sql::Serializer::sqlite(&schema.db);

        for table in &schema.db.tables {
            let stmt =
                serializer.serialize(&sql::Statement::create_table(table, &Capability::SQLITE));
            self.client
                .raw(&stmt, vec![])
                .await
                .map_err(into_core_error)?;

            for index in &table.indices {
                if index.primary_key {
                    continue;
                }
                let stmt = serializer.serialize(&sql::Statement::create_index(index));
                self.client
                    .raw(&stmt, vec![])
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
            )
            .await
            .map_err(into_core_error)?;

        let outcome = self
            .client
            .raw(
                "SELECT id FROM __toasty_migrations ORDER BY applied_at",
                vec![],
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
                .raw(statement, vec![])
                .await
                .map_err(into_core_error)?;
        }

        self.client
            .raw(
                "INSERT INTO __toasty_migrations (id, name, applied_at) \
                 VALUES (?1, ?2, datetime('now'))",
                vec![serde_json::json!(id as i64), serde_json::json!(name)],
            )
            .await
            .map_err(into_core_error)?;

        Ok(())
    }
}
