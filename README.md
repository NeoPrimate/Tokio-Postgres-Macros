# pg-macros

Proc-macros for working with `tokio-postgres`:

- **`from_row_derive`** — `#[derive(FromRow, ToRow)]` for mapping between Postgres
  rows and Rust structs. `ToRow` generates a `batch_insert(&[T], &client)`
  helper that issues one `INSERT … SELECT * FROM UNNEST(…)` per batch.
- **`from_row`** — runtime companion to the derive. Re-exports the derives plus
  the `query_as!` / `query!` / `query_scalar!` macros.
- **`query_as`** — proc-macro that parses the SQL at compile time, checks it
  against a live database (via `DATABASE_URL`), and emits a typed, bound
  call to `client.query(...)`.
- **`query_as_core`** — non-proc-macro shim that hosts the shared tokio
  runtime + Postgres client used by the proc-macro crates. (Proc-macro crates
  can only export proc-macros, so runtime infrastructure has to live in a
  separate non-`proc-macro` crate.)

## Usage

Add to `Cargo.toml`:

```toml
[dependencies]
from_row = { git = "https://github.com/YOUR_HANDLE/pg-macros" }
```

Then:

```rust
use from_row::{FromRow, ToRow, query_as, UuidBind};
use uuid::Uuid;

#[derive(FromRow, ToRow)]
struct User {
    id: Uuid,
    email: String,
}

let rows: Vec<User> = query_as!(
    User,
    "SELECT id, email FROM users WHERE organization_id = $1",
    org_id,
).fetch_all(&client).await?;
```

### Compile-time SQL checks

`query_as!` parses the SQL and, if `DATABASE_URL` is set at compile time,
validates the query against the live schema. Without `DATABASE_URL` it skips
validation and emits a runtime-bound query.

```bash
export DATABASE_URL="host=localhost user=erp password=erp dbname=erp_dev"
cargo build
```

### `UuidBind`

`query_as!` accepts any type that implements `UuidBind` as a UUID parameter.
`uuid::Uuid` is implemented by default. Newtype wrappers can opt in with a
one-liner:

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct UserId(uuid::Uuid);
impl from_row::UuidBind for UserId {}
```

Or use a companion `Id` derive macro that emits `impl UuidBind` alongside
other id-trait impls.

## History

Extracted from an in-tree `macros/` directory of a larger application that
migrated off Postgres. Kept here for projects that still need
compile-time-checked SQL.

## License

MIT OR Apache-2.0.
