# toasty-driver-d1

A [Toasty](https://github.com/tokio-rs/toasty) driver for
[Cloudflare D1](https://developers.cloudflare.com/d1/), speaking D1's HTTP API.

D1 is SQLite, so SQL generation reuses Toasty's SQLite serializer and
`Capability::SQLITE`; only the transport differs. Every operation is an HTTPS
round trip to `/accounts/{account}/d1/database/{database}/raw`, which means
the driver runs anywhere `reqwest` runs — no Workers runtime, no D1 binding.

> **Status: experimental.** It runs a real application against real D1, but
> the HTTP API cannot do transactions, and that rules out a large part of
> Toasty's feature set. Read [Limitations](#limitations) before adopting it.

## Usage

```toml
[dependencies]
toasty = "0.9"
toasty-driver-d1 = { git = "https://github.com/hirunefu/toasty-driver-d1" }
```

```rust
let driver = toasty_driver_d1::D1::new(account_id, database_id, api_token);
// or, from D1_ACCOUNT_ID / D1_DATABASE_ID / D1_API_TOKEN:
let driver = toasty_driver_d1::D1::from_env()?;

let db = toasty::Db::builder()
    .models(toasty::models!(crate::*))
    .build(driver)
    .await?;
```

The API token needs the account-level **D1 Edit** permission.

## What works

Verified against a live database by `tests/capabilities.rs`, one feature per
test:

| Feature | Status |
| --- | --- |
| Create / update / delete, one record per statement | ✅ |
| Queries by key, by field, `LIKE`, `starts_with`, `IN` | ✅ |
| `order_by` + `limit`, `count()` | ✅ |
| Relations queried per side (`author.books()`, FK lookup) | ✅ |
| Embedded structs and enums, documents, collections | ✅ |
| `Vec<u8>` and UUID keys (stored as real BLOBs) | ✅ |
| Upsert, auto-increment keys, composite keys, indices | ✅ |
| Multi-record `create!` | ❌ needs a transaction |
| Relation preload (`.include()`) | ❌ needs a transaction |
| Explicit transactions | ❌ |
| Integers beyond ±2^53 | ❌ rejected at bind time |
| `Timestamp` inside a `#[document]` value | ❌ |

The short version: **anything that is one statement works; anything Toasty
implements as several statements that must agree does not.**

## Limitations

These are properties of D1's HTTP API, not gaps that a future release closes.

**No transactions.** D1 rejects `BEGIN`, `COMMIT`, and `SAVEPOINT` outright,
even inside a single request:

> To execute a transaction, please use the `state.storage.transaction()` [...]
> APIs instead of the SQL BEGIN TRANSACTION or SAVEPOINT statements.

The driver therefore answers `Operation::Transaction` with
`unsupported_feature`. Anything Toasty implements on top of transactions —
batch creates, multi-statement rollback, `has_many` writes that touch several
tables — fails. Single-statement CRUD, queries, and relations that read across
tables all work.

**Integers are limited to ±2^53.** The API carries numbers as JSON, so larger
values are corrupted in transit: `i64::MAX` comes back as
`9223372036854776000` and the column's storage class flips from `integer` to
`real`. Rather than let that pass silently, binding rejects out-of-range
integers with an error naming the limit. Auto-increment ids stay far below it.

**No interactive migrations.** `apply_migration` runs each statement in its
own request, so a migration that fails midway leaves earlier statements
applied. Rerunning after a fix is the recovery path.

**Latency is HTTP latency.** Every statement is a separate HTTPS request.
This driver suits low-traffic applications and jobs, not hot paths.

Blobs, on the other hand, work: they cross as JSON arrays of byte values and
D1 stores them as real BLOBs, so `Vec<u8>` and UUID keys round-trip.

## Testing

Unit tests cover value conversion and the HTTP layer against a mock server,
and need no credentials:

```sh
cargo test
```

Toasty's official driver integration suite runs against a live database
behind the `live-tests` feature:

```sh
export D1_ACCOUNT_ID=... D1_API_TOKEN=...
export TOASTY_TEST_D1_DATABASE_ID=...   # a throwaway database
cargo test --features live-tests --test integration_suite
```

The suite creates and drops tables freely, hence the separate variable — do
not point it at a database you care about.

### Suite results

Against Toasty 0.9's suite (1348 generated tests): **717 pass, 631 fail**.
Every failure traces to one of three causes:

| Cause | Tests | Fixable |
| --- | --- | --- |
| Requires a transaction | 624 | No — D1 rejects `BEGIN`/`COMMIT` |
| Integer beyond ±2^53 | 4 | No — rejected at bind time rather than corrupted |
| Timestamp inside a `#[document]` value | 3 | Unknown — not yet investigated |

**Read that failure count carefully.** Most suite tests seed their fixtures
with a multi-record `create!`, so they fail before reaching the feature under
test: all six `filter_like` tests fail on D1, yet `LIKE` itself works. That is
why the capability table above comes from `tests/capabilities.rs`, which seeds
one record per statement, rather than from these totals. The suite number
measures how much of Toasty's surface assumes transactions — not how much of
it D1 can do.

## License

MIT
