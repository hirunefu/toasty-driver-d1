# Limitations: measurements and rejected designs

Background for the list in the [README](../README.md#what-this-driver-cannot-do).
Everything here was measured against a live D1 database rather than inferred
from documentation.

## 1. D1 refuses SQL transaction control

D1 rejects `BEGIN`, `COMMIT`, and `SAVEPOINT`, and says why:

> To execute a transaction, please use the `state.storage.transaction()` or
> `state.storage.transactionSync()` APIs instead of the SQL BEGIN TRANSACTION
> or SAVEPOINT statements. The JavaScript API is safer because it will
> automatically roll back on exceptions, and because it interacts correctly
> with Durable Objects' automatic atomic write coalescing.

The rejection is not about statements crossing requests. Sending
`BEGIN; INSERT ...; COMMIT;` as a single request fails with the same error, so
there is no way to open a transaction from the HTTP API at all.

The driver therefore answers `Operation::Transaction` with
`unsupported_feature`, and everything Toasty builds on that primitive — batch
creates, eager loading, multi-table writes, rollback — is unavailable.

Toasty's own DynamoDB driver rejects `Operation::Transaction` the same way, and
Toasty's guide states that `db.transaction()` returns an error there, so this
is a shape the ecosystem already accommodates.

## 2. D1 *does* have atomic multi-statement execution

Refusing `BEGIN` does not mean D1 lacks transactions — it manages them itself.
Several statements in one request are executed, and executed atomically:

| Request | Result |
| --- | --- |
| `INSERT (1,'a'); INSERT (2,'b');` | both applied, two result sets returned |
| `INSERT (3,'c'); INSERT (4,NULL);` (second violates `NOT NULL`) | request fails **and row 3 is absent** |

The second row is the important one: the failing statement rolled back its
predecessor. This is the same primitive behind `db.batch()` in the Workers
binding.

So the capability exists at the transport layer. What follows is why the
driver still cannot expose it through Toasty 0.9.

## 3. How other ORMs handle this

**Drizzle ORM** implements `transaction()` for D1 by emitting the literal SQL
D1 rejects ([`drizzle-orm/src/d1/session.ts`](https://github.com/drizzle-team/drizzle-orm/blob/main/drizzle-orm/src/d1/session.ts)):

```ts
override async transaction<T>(transaction, config?) {
    const tx = new D1Transaction('async', this.dialect, this, this.schema);
    await this.run(sql.raw(`begin${config?.behavior ? ' ' + config.behavior : ''}`));
    try {
        const result = await transaction(tx);
        await this.run(sql`commit`);
        return result;
    } catch (err) {
        await this.run(sql`rollback`);
        throw err;
    }
}
```

Nested transactions use `savepoint` / `rollback to savepoint`, which D1 rejects
as well. The method exists in the type system but fails at runtime; Drizzle
carries an open issue, *"[BUG]: Cloudflare D1 transaction not supported"*.

Drizzle's working path is `batch()`, where the caller lists the statements up
front and the ORM submits them together. That is the ecosystem's consensus
answer: **expose batching explicitly instead of emulating interactive
transactions.**

The difference for Toasty is that its transaction boundaries are emitted by
the engine, not by the user — which is what makes the next section worth
attempting at all.

## 4. Two designs tried and rejected

### Buffer statements until commit — deadlocks

Toasty issues a clean sequence for a multi-record create, with no reads
interleaved between the writes:

```
transaction  Start { isolation: None, read_only: false, mode: Default }
query_sql    INSERT INTO "books" (...) VALUES (NULL, ?1, ?2, ?3) RETURNING "id"
query_sql    INSERT INTO "books" (...) VALUES (NULL, ?1, ?2, ?3) RETURNING "id"
transaction  Commit
```

That shape maps exactly onto an atomic multi-statement request: buffer the
statements at `Start`, send them as one request at `Commit`, and discard the
buffer on `Rollback` — a rollback so complete that nothing ever ran.

The obstacle is the `RETURNING "id"` on each insert. The driver cannot know
the ids until the request is sent, so each buffered statement has to return a
result that resolves later. `stmt::ValueStream::from_stream` makes that
expressible: return a stream backed by a `oneshot` channel, and fulfil the
channels once the batch executes.

A prototype was built along these lines. **It deadlocks.** Toasty's engine
awaits each statement's rows before issuing the next, so the first deferred
stream never resolves — the `Commit` that would fulfil it is never reached.
The test hung for over ten minutes before being killed.

### Treat transaction boundaries as no-ops — unsafe

Accepting `Start`/`Commit`/`Rollback` as no-ops and writing eagerly would make
batch creates appear to work. It also silently drops atomicity: a failure
partway through leaves the earlier writes committed, with no error to
distinguish that from success. Rejected — a driver that quietly abandons a
guarantee it appears to provide is worse than one that refuses.

## 5. Where the fix belongs

Upstream. Toasty already has a design for it:
[`docs/dev/design/atomic-batches.md`](https://github.com/tokio-rs/toasty/blob/main/docs/dev/design/atomic-batches.md)
proposes `Capability::transaction_delivery` with three modes:

* `Unsupported` — no atomic multi-write
* `Streamed` — today's SQL drivers, statements one at a time under engine-controlled `BEGIN`/`COMMIT`
* `WriteSet` — the driver receives the whole atomic group as **one** operation to commit or cancel together

`WriteSet` was motivated by DynamoDB's `transact_write_items()`, but D1's
multi-statement request fits it exactly, and it sidesteps the deadlock above
because the driver is handed every statement at once instead of being fed them
one at a time.

The design is not implemented in Toasty 0.9, nor on upstream `main` — only the
document exists. Until it lands, this driver rejects transactions.

## 6. Integers are limited to ±2^53

D1's HTTP API carries numbers as JSON. Values beyond 2^53 do not merely lose
precision on the way back; the stored column changes storage class:

| Sent | Returned | `typeof()` |
| --- | --- | --- |
| `9223372036854775807` (`i64::MAX`) | `9223372036854776000` | `real` |
| `-9223372036854775808` (`i64::MIN`) | `-9223372036854776000` | `real` |
| `9007199254740992` (2^53) | `9007199254740992` | `integer` |
| `9007199254740993` (2^53 + 1) | `9007199254740992` | `integer` |

Note the last row: truncation with no error anywhere. The driver therefore
rejects out-of-range integers when binding, so the failure is loud and
attributable instead of a wrong number discovered later. Auto-increment ids
stay far below the limit.

`u64` is a special case. SQLite has no unsigned integer type, so — like the
SQLite driver — the driver wraps `u64` into `i64` and casts the bit pattern
back when decoding. `u64::MAX` therefore round-trips correctly as `-1`, while
a mid-range value such as `2^60` is rejected.

## 7. Blobs work

The one limitation that turned out not to be one. Passing a JSON array of byte
values binds a real BLOB, and D1 returns it in the same shape:

```
bind [1,2,3]  →  typeof() = 'blob',  hex() = '010203',  returned as [1,2,3]
```

So `Vec<u8>` columns and UUID keys round-trip exactly. Supporting this
recovered 311 tests in the integration suite.

## 8. Reading the integration suite results

Against Toasty 0.9's suite (1348 generated tests): **717 pass, 631 fail**.

| Cause | Tests | Fixable |
| --- | --- | --- |
| Requires a transaction | 624 | No — D1 rejects `BEGIN`/`COMMIT` |
| Integer beyond ±2^53 | 4 | No — rejected at bind time rather than corrupted |
| `Timestamp` inside a `#[document]` value | 3 | Unknown — not yet investigated |

**That failure count overstates the gap.** Most suite tests seed their
fixtures with a multi-record `create!`, so they fail before reaching the
feature under test. All six `filter_like` tests fail on D1, yet `LIKE` itself
works fine.

This is why the capability table in the README comes from
[`tests/capabilities.rs`](../tests/capabilities.rs), which seeds one record per
statement and probes one feature per test, rather than from these totals. The
suite number measures how much of Toasty's surface assumes transactions — not
how much of it D1 can do.
