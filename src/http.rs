//! Minimal client for D1's `/raw` query endpoint.
//!
//! `/raw` (not `/query`) because it returns rows as positional arrays,
//! matching how the query plan addresses result columns by index.

use serde::Deserialize;

use crate::error::D1Error;

pub(crate) const DEFAULT_BASE_URL: &str = "https://api.cloudflare.com/client/v4";

#[derive(Clone)]
pub(crate) struct D1Client {
    http: reqwest::Client,
    endpoint: String,
    token: String,
}

/// One statement's outcome from `/raw`.
#[derive(Debug, Default)]
pub(crate) struct RawOutcome {
    pub(crate) rows: Vec<Vec<serde_json::Value>>,
    pub(crate) changes: u64,
}

#[derive(Deserialize)]
struct Envelope {
    success: bool,
    #[serde(default)]
    errors: Vec<ApiError>,
    #[serde(default)]
    result: Vec<StatementResult>,
}

#[derive(Deserialize)]
struct ApiError {
    code: i64,
    message: String,
}

#[derive(Deserialize)]
struct StatementResult {
    #[serde(default)]
    results: Option<RawResults>,
    #[serde(default)]
    meta: Option<Meta>,
}

#[derive(Deserialize)]
struct RawResults {
    #[serde(default)]
    rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
struct Meta {
    #[serde(default)]
    changes: u64,
}

/// Errors split by transport vs. backend so the caller can classify
/// connection loss for the pool (see toasty's driver error contract).
pub(crate) enum HttpError {
    Transport(reqwest::Error),
    Api(D1Error),
}

impl D1Client {
    pub(crate) fn new(base_url: &str, account_id: &str, database_id: &str, token: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: format!(
                "{}/accounts/{}/d1/database/{}/raw",
                base_url.trim_end_matches('/'),
                account_id,
                database_id
            ),
            token: token.to_string(),
        }
    }

    pub(crate) async fn raw(
        &self,
        sql: &str,
        params: Vec<serde_json::Value>,
    ) -> Result<RawOutcome, HttpError> {
        let response = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "sql": sql, "params": params }))
            .send()
            .await
            .map_err(HttpError::Transport)?;

        let status = response.status();
        let envelope: Envelope = response.json().await.map_err(HttpError::Transport)?;

        if !envelope.success {
            let detail = envelope
                .errors
                .iter()
                .map(|e| format!("{}: {}", e.code, e.message))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(HttpError::Api(D1Error::new(format!(
                "D1 query failed (HTTP {status}): {detail}"
            ))));
        }

        // One statement in, one result out.
        let statement = envelope
            .result
            .into_iter()
            .next()
            .ok_or_else(|| HttpError::Api(D1Error::new("D1 returned no statement result")))?;

        Ok(RawOutcome {
            rows: statement.results.map(|r| r.rows).unwrap_or_default(),
            changes: statement.meta.map(|m| m.changes).unwrap_or_default(),
        })
    }
}

impl std::fmt::Debug for D1Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The bearer token must never reach logs.
        f.debug_struct("D1Client")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn client(server: &MockServer) -> D1Client {
        D1Client::new(&server.uri(), "acct", "dbid", "sekrit")
    }

    #[tokio::test]
    async fn raw_posts_sql_with_bearer_auth_and_parses_rows() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/accounts/acct/d1/database/dbid/raw"))
            .and(header("authorization", "Bearer sekrit"))
            .and(body_partial_json(serde_json::json!({
                "sql": "SELECT id, title FROM todos WHERE id = ?1",
                "params": ["1"],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": [{
                    "results": { "columns": ["id", "title"], "rows": [[1, "buy milk"]] },
                    "success": true,
                    "meta": { "changes": 0 }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let outcome = client(&server)
            .raw(
                "SELECT id, title FROM todos WHERE id = ?1",
                vec![serde_json::json!("1")],
            )
            .await
            .unwrap_or_else(|_| panic!("request should succeed"));

        assert_eq!(outcome.rows.len(), 1);
        assert_eq!(outcome.rows[0][0], serde_json::json!(1));
        assert_eq!(outcome.rows[0][1], serde_json::json!("buy milk"));
    }

    #[tokio::test]
    async fn raw_reports_write_counts_from_meta() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": [{ "results": { "columns": [], "rows": [] }, "success": true, "meta": { "changes": 3 } }]
            })))
            .mount(&server)
            .await;

        let outcome = client(&server)
            .raw("DELETE FROM todos", vec![])
            .await
            .unwrap_or_else(|_| panic!("request should succeed"));

        assert_eq!(outcome.changes, 3);
    }

    #[tokio::test]
    async fn raw_surfaces_api_errors_with_code_and_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "success": false,
                "errors": [{ "code": 7500, "message": "no such table: nope" }],
                "result": []
            })))
            .mount(&server)
            .await;

        let Err(err) = client(&server).raw("SELECT * FROM nope", vec![]).await else {
            panic!("request should fail");
        };

        match err {
            HttpError::Api(e) => {
                let msg = e.to_string();
                assert!(msg.contains("7500"), "message was: {msg}");
                assert!(msg.contains("no such table"), "message was: {msg}");
            }
            HttpError::Transport(_) => panic!("expected an API error, got transport"),
        }
    }
}
