//! Dispatch between the two ways of reaching a D1 database.
//!
//! Both answer the same two questions — run this statement, run this batch —
//! so everything above this module is written once. Which one is compiled in
//! is a feature choice: `rest` works anywhere, `binding` only inside a Worker.

use serde_json::Value as Json;

use crate::error::TransportError;
use crate::outcome::{RawOutcome, Want};

#[derive(Debug, Clone)]
pub(crate) enum Transport {
    #[cfg(feature = "rest")]
    Rest(crate::http::D1Client),
    #[cfg(feature = "binding")]
    Binding(crate::binding::D1Binding),
}

impl Transport {
    pub(crate) async fn raw(
        &self,
        sql: &str,
        params: Vec<Json>,
        want: Want,
    ) -> Result<RawOutcome, TransportError> {
        match self {
            // The HTTP API reports rows and the change count together, so it
            // has no use for `want`.
            #[cfg(feature = "rest")]
            Transport::Rest(client) => {
                let _ = want;
                client.raw(sql, params).await
            }
            #[cfg(feature = "binding")]
            Transport::Binding(binding) => binding.raw(sql, params, want).await,
        }
    }

    pub(crate) async fn raw_batch(&self, sql: &str) -> Result<Vec<RawOutcome>, TransportError> {
        match self {
            #[cfg(feature = "rest")]
            Transport::Rest(client) => client.raw_batch(sql).await,
            #[cfg(feature = "binding")]
            Transport::Binding(binding) => binding.raw_batch(sql).await,
        }
    }
}
