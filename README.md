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

Four things fail, every time. Each was measured against a live database. Two
are properties of D1 itself and will not change; two are gaps in Toasty that
an upstream change could close. For the evidence, what D1 can do instead, how
other ORMs handle the same wall, and the designs that were tried and rejected,
see **[docs/limitations.md](docs/limitations.md)**.

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

**3. Relation preload (`.include()`).** Eager loading runs its reads under one
transaction, for a consistent snapshot. Query each side separately —
`author.books()` and foreign-key lookups both work, at the cost of an extra
round trip and of that snapshot.

```rust
Author::filter_by_id(id).include(Author::fields().books()).get(&mut db).await
// Error: unsupported feature: ... interactive transactions
```

**4. Integers beyond ±2^53.** The API carries numbers as JSON, so anything
larger is corrupted in transit — `i64::MAX` comes back as
`9223372036854776000` and the column's storage class flips to `real`
([measurements](docs/limitations.md#6-integers-are-limited-to-253)). The driver
refuses such values rather than let that happen silently.

```rust
Item::create().big(i64::MAX).exec(&mut db).await
// Error: integer 9223372036854775807 exceeds ±2^53, the range D1's JSON API
//        carries without loss of precision
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

All verified against a live database. The **probe** column says where: `cap`
is [`tests/capabilities.rs`](tests/capabilities.rs), which exercises one
feature per test; `suite` is Toasty's own driver integration suite.

| Feature | Status | Probe |
| --- | --- | --- |
| Create / update / delete, one record per statement | ✅ | cap |
| Queries by key, by field, `LIKE`, `starts_with`, `IN` | ✅ | cap |
| `order_by` + `limit`, `count()` | ✅ | cap |
| Relations queried per side (`author.books()`, FK lookup) | ✅ | cap |
| Embedded structs and enums | ✅ | suite |
| Documents and collections, including temporal values | ✅ | suite |
| `Vec<u8>` and UUID keys | ✅ | suite |
| Upsert, auto-increment keys, composite keys, indices | ✅ | suite |
| Multi-record `create!` | ❌ | cap |
| Relation preload (`.include()`) | ❌ | cap |
| Explicit transactions | ❌ | suite |
| Integers beyond ±2^53 | ❌ | suite |

The short version: **anything that is one statement works; anything Toasty
implements as several statements that must agree does not.**

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

The suite reports **720 pass, 628 fail**. That count overstates the gap: most
of its tests seed fixtures with a multi-record `create!` and so fail before
reaching the feature under test — all six `filter_like` tests fail on D1, yet
`LIKE` itself works. See
[docs/limitations.md](docs/limitations.md#7-reading-the-integration-suite-results)
for the breakdown.

`tests/capabilities.rs` is the counterpart, and the source of the table above:
one feature per test, seeded one record per statement.

```sh
cargo test --features live-tests --test capabilities
```

## License

MIT
