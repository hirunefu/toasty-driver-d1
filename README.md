# toasty-driver-d1

A [Toasty](https://github.com/tokio-rs/toasty) driver for
[Cloudflare D1](https://developers.cloudflare.com/d1/), speaking D1's HTTP API.

D1 is SQLite, so SQL generation reuses Toasty's SQLite serializer and
`Capability::SQLITE`; only the transport differs. Every operation is an HTTPS
round trip to `/accounts/{account}/d1/database/{database}/raw`, which means
the driver runs anywhere `reqwest` runs — no Workers runtime, no D1 binding.

> **Status: experimental.** It runs a real application against real D1, but
> the HTTP API offers no interactive transactions, and that rules out a large
> part of Toasty's feature set. Read the next section before adopting it.

## What this driver cannot do

Five things fail, every time, by design. Each was measured against a live
database — none is a "not yet implemented".

**1. Explicit transactions.** D1 rejects `BEGIN`, `COMMIT`, and `SAVEPOINT`
as SQL, so `db.transaction()` cannot be honoured.

```rust
db.transaction(|tx| async move { /* ... */ }).await
// Error: unsupported feature: the D1 HTTP API does not support
//        interactive transactions
```

**2. Multi-record `create!`.** Toasty wraps a multi-record insert in a
transaction, so it fails for the same reason. Insert one record per statement
instead.

```rust
toasty::create!(Book::[ { title: "One" }, { title: "Two" } ]).exec(&mut db).await
// Error: unsupported feature: ... interactive transactions
```

**3. Relation preload (`.include()`).** Eager loading issues several
statements under one transaction. Query each side separately — `author.books()`
and foreign-key lookups both work, at the cost of an extra round trip.

```rust
Author::filter_by_id(id).include(Author::fields().books()).get(&mut db).await
// Error: unsupported feature: ... interactive transactions
```

**4. Integers beyond ±2^53.** The API carries numbers as JSON, so anything
larger is corrupted in transit — `i64::MAX` comes back as
`9223372036854776000` and the column's storage class flips to `real`. The
driver refuses such values rather than let that happen silently.

```rust
Item::create().big(i64::MAX).exec(&mut db).await
// Error: integer 9223372036854775807 exceeds ±2^53, the range D1's JSON API
//        carries without loss of precision
```

**5. `Timestamp` inside a `#[document]` value.** Serializing an embedded
document that contains a timestamp fails. Timestamps in ordinary columns are
fine.

```
Error: serialize document value: cannot encode Timestamp(..) as JSON
```

Two more properties are worth knowing before adopting the driver, though
neither is an outright failure:

- **A failed migration is not rolled back.** `apply_migration` runs each
  statement in its own request, so a migration that fails midway leaves the
  earlier statements applied. Rerun after fixing it.
- **Every statement is an HTTPS round trip.** Suitable for low-traffic
  applications and jobs, not hot paths.

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

## Why those limits exist

Failures 1–3 above all reduce to one thing: D1 refuses SQL-level transaction
control, and says so explicitly.

> To execute a transaction, please use the `state.storage.transaction()` [...]
> APIs instead of the SQL BEGIN TRANSACTION or SAVEPOINT statements.

So the driver answers `Operation::Transaction` with `unsupported_feature`, and
everything Toasty builds on that primitive — batch creates, eager loading,
multi-table writes, rollback — goes with it.

### Why not emulate transactions?

D1's HTTP API *can* run several statements atomically — send them in one
request and a failure anywhere rolls the whole request back. That is the same
primitive behind `db.batch()` in the Workers binding, and it is how Drizzle
ORM supports multi-statement writes on D1. (Drizzle's `transaction()` emits
literal `begin`/`commit`, which D1 rejects; it has been an open bug there.)

Two designs were tried against that primitive and rejected:

- **Buffer statements until commit, returning lazy result streams.** Toasty's
  engine awaits each statement's rows before issuing the next, so deferring
  results deadlocks. Measured, not assumed.
- **Treat transaction boundaries as no-ops and write eagerly.** This makes
  batches "work" while silently dropping atomicity — a mid-batch failure
  leaves partial data with no error. Not worth the correctness.

The clean fix belongs upstream. Toasty has a design for it —
`Capability::transaction_delivery` with a `WriteSet` mode, where a driver
receives an atomic group as one operation instead of a statement stream
([docs/dev/design/atomic-batches.md](https://github.com/tokio-rs/toasty/blob/main/docs/dev/design/atomic-batches.md),
motivated by DynamoDB). D1's HTTP API fits `WriteSet` exactly. The design is
not implemented in Toasty 0.9, so until it lands this driver rejects
transactions, matching what Toasty's own DynamoDB driver does.

Blobs, by contrast, were a pleasant surprise: passed as JSON arrays of byte
values, D1 stores them as real BLOBs (`typeof()` reports `blob`), so `Vec<u8>`
and UUID keys round-trip exactly.

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
