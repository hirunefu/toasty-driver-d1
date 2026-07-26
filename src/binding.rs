//! D1 over the Workers binding.
//!
//! Same statements as the HTTP transport, reached differently: the binding is
//! a JavaScript object the runtime hands to the isolate, so there is no
//! endpoint, no token, and no trip out to Cloudflare's REST API — which is
//! where that transport spends nearly all of its latency.
//!
//! Everything here is `!Send`, because JavaScript values are. Toasty's
//! `Driver` and `Connection` traits require `Send` futures, so the handle and
//! every future cross that boundary through `worker::send`. That is sound
//! rather than a cheat: a Worker isolate is single-threaded, so nothing is
//! ever actually moved between threads.

use std::rc::Rc;

use serde_json::Value as Json;
use wasm_bindgen::JsValue;
use worker::send::{SendFuture, SendWrapper};

use crate::error::{D1Error, TransportError};
use crate::outcome::{RawOutcome, Want, split_statements};

pub(crate) struct D1Binding {
    /// `Rc` because `D1Database` is not `Clone` and toasty builds a fresh
    /// transport per connection; `SendWrapper` because neither it nor the
    /// `Rc` is `Send`, which `Driver` demands.
    inner: SendWrapper<Rc<worker::D1Database>>,
}

impl std::fmt::Debug for D1Binding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("D1Binding").finish_non_exhaustive()
    }
}

impl Clone for D1Binding {
    fn clone(&self) -> Self {
        Self {
            inner: SendWrapper::new(Rc::clone(&self.inner)),
        }
    }
}

impl D1Binding {
    pub(crate) fn new(database: worker::D1Database) -> Self {
        Self {
            inner: SendWrapper::new(Rc::new(database)),
        }
    }

    /// Runs one statement.
    ///
    /// `want` decides which call is made, because the binding splits rows and
    /// the change count across two of them — see [`Want`].
    pub(crate) async fn raw(
        &self,
        sql: &str,
        params: Vec<Json>,
        want: Want,
    ) -> Result<RawOutcome, TransportError> {
        let db = SendWrapper::new(Rc::clone(&self.inner));
        let sql = sql.to_string();

        // The prepared statement is a JavaScript value and is held across the
        // await, so the whole block crosses the Send boundary as one future
        // rather than each await individually.
        SendFuture::new(async move {
            let sql = sql.as_str();
            let bound: Vec<JsValue> = params.iter().map(json_to_js).collect();

            let statement = db.prepare(sql).bind(&bound).map_err(|err| {
                TransportError::Api(D1Error::new(format!("D1 bind failed: {err}")))
            })?;

            match want {
                Want::Rows => {
                    let rows = statement
                        .raw::<Json>()
                        .await
                        .map_err(|err| classify(sql, &err))?;
                    Ok(RawOutcome { rows, changes: 0 })
                }
                Want::Changes => {
                    let result = statement.run().await.map_err(|err| classify(sql, &err))?;
                    Ok(RawOutcome {
                        rows: Vec::new(),
                        changes: changes_of(&result),
                    })
                }
            }
        })
        .await
    }

    /// Runs several statements atomically.
    ///
    /// `D1Database::batch` is the binding's equivalent of submitting one
    /// multi-statement request to the HTTP API: D1 applies the whole batch or
    /// none of it, which is what `Capability::atomic_write_batch` promises.
    ///
    /// The statements arrive already inlined, for the same reason as on the
    /// HTTP transport: `?N` placeholders are numbered per statement.
    pub(crate) async fn raw_batch(&self, sql: &str) -> Result<Vec<RawOutcome>, TransportError> {
        let db = SendWrapper::new(Rc::clone(&self.inner));
        let sql = sql.to_string();

        SendFuture::new(async move {
            let sql = sql.as_str();
            let statements: Vec<_> = split_statements(sql).map(|stmt| db.prepare(stmt)).collect();

            let results = db
                .batch(statements)
                .await
                .map_err(|err| classify(sql, &err))?;

            Ok(results
                .iter()
                .map(|result| RawOutcome {
                    rows: Vec::new(),
                    changes: changes_of(result),
                })
                .collect())
        })
        .await
    }
}

fn changes_of(result: &worker::D1Result) -> u64 {
    result
        .meta()
        .ok()
        .flatten()
        .and_then(|meta| meta.changes)
        .unwrap_or(0)
        .max(0) as u64
}

/// Classifies a binding failure.
///
/// The binding surfaces both "the database rejected this" and "the call could
/// not be made" as one error type, so the message is all there is to go on.
/// Misclassifying a rejected statement as connection loss would have the pool
/// throw away a perfectly good slot, so anything recognisably from SQLite is
/// treated as an API error and only the rest as a lost connection.
fn classify(sql: &str, err: &worker::Error) -> TransportError {
    let message = format!("D1 statement failed ({sql}): {err}");
    if err.to_string().contains("D1_ERROR") {
        TransportError::Api(D1Error::new(message))
    } else {
        TransportError::Lost(D1Error::new(message))
    }
}

/// Converts a JSON parameter into the JavaScript value D1 binds.
///
/// Only the shapes `value::param_to_json` produces appear here: null,
/// booleans, numbers, and strings. D1 rejects anything else as a bound
/// parameter, and the driver documents `Bytes` columns as unsupported.
fn json_to_js(value: &Json) -> JsValue {
    match value {
        Json::Null => JsValue::NULL,
        Json::Bool(b) => JsValue::from_bool(*b),
        Json::Number(n) => n.as_f64().map_or(JsValue::NULL, JsValue::from_f64),
        Json::String(s) => JsValue::from_str(s),
        // Unreachable for parameters the driver builds; encoded rather than
        // dropped so a future change fails loudly at D1 instead of silently
        // binding null.
        other => JsValue::from_str(&other.to_string()),
    }
}
