//! Runs toasty's official driver integration suite against a live D1
//! database.
//!
//! Behind the `live-tests` feature because every test here needs credentials
//! and a real database:
//!
//! ```sh
//! cargo test --features live-tests --test integration_suite
//! ```
//!
//! Requires `D1_ACCOUNT_ID`, `D1_API_TOKEN`, and `TOASTY_TEST_D1_DATABASE_ID`.
//! The suite creates and drops tables freely, so that database ID must point
//! at a throwaway database — deliberately a separate variable from the
//! `D1_DATABASE_ID` an application would use.
#![cfg(feature = "live-tests")]

use toasty_driver_d1::D1;

struct D1Setup;

impl D1Setup {
    fn config() -> (String, String, String) {
        let get = |key: &str| {
            std::env::var(key)
                .unwrap_or_else(|_| panic!("{key} must be set to run the D1 integration suite"))
        };
        (
            get("D1_ACCOUNT_ID"),
            get("TOASTY_TEST_D1_DATABASE_ID"),
            get("D1_API_TOKEN"),
        )
    }

    /// Executes one statement over the D1 HTTP API.
    ///
    /// The suite only needs to drop tables, which the `Driver` trait does not
    /// expose, so the harness talks to the API directly rather than widening
    /// the crate's public surface.
    async fn exec(sql: &str) {
        let (account, database, token) = Self::config();
        let response = reqwest::Client::new()
            .post(format!(
                "https://api.cloudflare.com/client/v4/accounts/{account}/d1/database/{database}/raw"
            ))
            .bearer_auth(token)
            .json(&serde_json::json!({ "sql": sql, "params": [] }))
            .send()
            .await
            .expect("D1 request failed");
        assert!(
            response.status().is_success(),
            "D1 rejected `{sql}`: {}",
            response.text().await.unwrap_or_default()
        );
    }
}

#[async_trait::async_trait]
impl toasty_driver_integration_suite::Setup for D1Setup {
    fn driver(&self) -> Box<dyn toasty_core::driver::Driver> {
        let (account, database, token) = Self::config();
        Box::new(D1::new(account, database, token))
    }

    async fn delete_table(&self, name: &str) {
        Self::exec(&format!("DROP TABLE IF EXISTS \"{name}\"")).await;
    }
}

// D1 is SQLite, so the capability flags mirror the SQLite driver's.
toasty_driver_integration_suite::generate_driver_tests!(
    D1Setup,
    native_decimal: false,
    bigdecimal_implemented: false,
    decimal_arbitrary_precision: false,
    native_timestamp: false,
    native_date: false,
    native_time: false,
    native_datetime: false,
    native_ilike: false,
    native_json: false,
    native_jsonb: false,
    native_array: false,
    native_enum: false,
    vec_scalar: true,
    document_collections: true,
    vec_remove: false,
    vec_pop: false,
    vec_remove_at: false,
    test_connection_pool: false,
);
