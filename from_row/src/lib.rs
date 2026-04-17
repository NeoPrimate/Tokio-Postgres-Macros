//! Runtime support for `#[derive(FromRow)]`.

pub mod ty_match;

pub use from_row_derive::{FromRow, ToRow};
pub use query_as::{query, query_as, query_scalar};

/// Marker trait: types that can be passed as a UUID bind to `query_as!`.
///
/// Implement this (or `#[derive(Id)]` from a companion crate) for newtype
/// wrappers around `Uuid` so the generated query code accepts them.
/// Deliberately not blanket-implemented — opt-in per type to prevent coherence
/// surprises.
pub trait UuidBind {}

impl UuidBind for uuid::Uuid {}

// ─── ToRow trait ─────────────────────────────────────────────────────────────
//
// Implemented automatically by `#[derive(ToRow)]`.
// `batch_insert` collects each field into a typed Vec and issues a single
// INSERT … SELECT * FROM UNNEST(…) — one round-trip for N rows.

#[allow(async_fn_in_trait)]
pub trait ToRow: Sized {
    async fn batch_insert(
        rows: &[Self],
        client: &impl GenericClient,
    ) -> Result<u64, tokio_postgres::Error>;
}

/// Convenience macro: `batch_insert!(Type, &client, &rows).await`
#[macro_export]
macro_rules! batch_insert {
    ($T:ty, $client:expr, $rows:expr) => {
        <$T as from_row::ToRow>::batch_insert($rows, $client)
    };
}

// ─── FromRow trait ────────────────────────────────────────────────────────────

/// Implemented automatically by `#[derive(FromRow)]`.
/// The generated `from_row` method calls `row.get("column_name")` with static
/// string keys — zero allocations, identical to handwritten tokio-postgres code.
pub trait FromRow: Sized {
    fn from_row(row: &tokio_postgres::Row) -> Self;
}

// ─── GenericClient trait ──────────────────────────────────────────────────────
//
// Abstracts over clients that may or may not have a prepared statement cache.
// `Query` and `Scalar` always call `prepare_cached` — if the impl has a cache
// it will be a cache hit after the first call; if not, it prepares each time.

#[allow(async_fn_in_trait)]
pub trait GenericClient {
    /// Return a prepared statement, using a per-connection cache if available.
    ///
    /// - `tokio_postgres::Client` — no cache: prepares on every call.
    /// - `CachedClient` — HashMap cache: prepares once per connection lifetime.
    /// - `deadpool_postgres::Object` (feature `deadpool`) — uses deadpool's
    ///   built-in per-connection `StatementCache`.
    async fn prepare_cached(
        &self,
        sql: &'static str,
    ) -> Result<tokio_postgres::Statement, tokio_postgres::Error>;

    /// The underlying `tokio_postgres::Client` used to execute queries.
    fn as_client(&self) -> &tokio_postgres::Client;
}

impl GenericClient for tokio_postgres::Client {
    #[inline]
    async fn prepare_cached(
        &self,
        sql: &'static str,
    ) -> Result<tokio_postgres::Statement, tokio_postgres::Error> {
        self.prepare(sql).await
    }

    #[inline]
    fn as_client(&self) -> &tokio_postgres::Client {
        self
    }
}

// ─── deadpool_postgres::Object impl (feature = "deadpool") ───────────────────

#[cfg(feature = "deadpool")]
impl GenericClient for deadpool_postgres::Object {
    async fn prepare_cached(
        &self,
        sql: &'static str,
    ) -> Result<tokio_postgres::Statement, tokio_postgres::Error> {
        // Deref: Object → ClientWrapper, then call ClientWrapper::prepare_cached.
        let wrapper: &deadpool_postgres::ClientWrapper = self;
        wrapper.prepare_cached(sql).await
    }

    #[inline]
    fn as_client(&self) -> &tokio_postgres::Client {
        // Double deref: Object → ClientWrapper → tokio_postgres::Client.
        self
    }
}

#[cfg(feature = "deadpool")]
impl GenericClient for deadpool_postgres::Transaction<'_> {
    async fn prepare_cached(
        &self,
        sql: &'static str,
    ) -> Result<tokio_postgres::Statement, tokio_postgres::Error> {
        deadpool_postgres::Transaction::prepare_cached(self, sql).await
    }

    #[inline]
    fn as_client(&self) -> &tokio_postgres::Client {
        // Transaction derefs to tokio_postgres::Transaction which has client()
        self.client()
    }
}

// ─── Query<'q, T, N> — zero-cost struct query builder ────────────────────────
//
// `N` is the bind count, a const generic known at macro expansion time.
// `params` is a stack-allocated array of fat pointers — no heap allocation.
// All methods are `#[inline]` so the compiler folds them into the caller's
// async state machine, producing the same IR as a direct `client.query()` call.

pub struct Query<'q, T, const N: usize> {
    sql: &'static str,
    params: [&'q (dyn tokio_postgres::types::ToSql + Sync + 'q); N],
    _marker: std::marker::PhantomData<fn() -> T>,
}

impl<'q, T, const N: usize> Query<'q, T, N> {
    #[doc(hidden)]
    #[inline(always)]
    pub fn new(
        sql: &'static str,
        params: [&'q (dyn tokio_postgres::types::ToSql + Sync + 'q); N],
    ) -> Self {
        Self {
            sql,
            params,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<'q, T: FromRow, const N: usize> Query<'q, T, N> {
    /// Fetch a single row. Errors if zero or more than one row is returned.
    #[inline]
    pub async fn fetch_one(self, client: &impl GenericClient) -> Result<T, tokio_postgres::Error> {
        let stmt = client.prepare_cached(self.sql).await?;
        let row = client.as_client().query_one(&stmt, &self.params).await?;
        Ok(T::from_row(&row))
    }

    /// Fetch an optional row. Returns `None` if no row is found.
    #[inline]
    pub async fn fetch_opt(
        self,
        client: &impl GenericClient,
    ) -> Result<Option<T>, tokio_postgres::Error> {
        let stmt = client.prepare_cached(self.sql).await?;
        let row = client.as_client().query_opt(&stmt, &self.params).await?;
        Ok(row.as_ref().map(T::from_row))
    }

    /// Fetch all rows into a `Vec`.
    #[inline]
    pub async fn fetch_all(
        self,
        client: &impl GenericClient,
    ) -> Result<Vec<T>, tokio_postgres::Error> {
        let stmt = client.prepare_cached(self.sql).await?;
        let rows = client.as_client().query(&stmt, &self.params).await?;
        Ok(rows.iter().map(T::from_row).collect())
    }

    /// Stream rows lazily. The query is sent on `.await`; rows are decoded on demand.
    #[inline]
    pub async fn fetch(
        self,
        client: &impl GenericClient,
    ) -> Result<
        impl futures_util::Stream<Item = Result<T, tokio_postgres::Error>>,
        tokio_postgres::Error,
    > {
        let stmt = client.prepare_cached(self.sql).await?;
        let stream = client
            .as_client()
            .query_raw(&stmt, self.params.iter().copied())
            .await?;
        Ok(futures_util::TryStreamExt::map_ok(stream, |row| {
            T::from_row(&row)
        }))
    }
}

// ─── Scalar<'q, N> — zero-cost scalar query builder ──────────────────────────

pub struct Scalar<'q, const N: usize> {
    sql: &'static str,
    params: [&'q (dyn tokio_postgres::types::ToSql + Sync + 'q); N],
}

impl<'q, const N: usize> Scalar<'q, N> {
    #[doc(hidden)]
    #[inline(always)]
    pub fn new(
        sql: &'static str,
        params: [&'q (dyn tokio_postgres::types::ToSql + Sync + 'q); N],
    ) -> Self {
        Self { sql, params }
    }

    /// Fetch the first column of a single row. Errors if zero or more than one row.
    #[inline]
    pub async fn fetch_one<T>(self, client: &impl GenericClient) -> Result<T, tokio_postgres::Error>
    where
        T: for<'a> tokio_postgres::types::FromSql<'a>,
    {
        let stmt = client.prepare_cached(self.sql).await?;
        let row = client.as_client().query_one(&stmt, &self.params).await?;
        Ok(row.get(0))
    }

    /// Fetch the first column of an optional row.
    #[inline]
    pub async fn fetch_opt<T>(
        self,
        client: &impl GenericClient,
    ) -> Result<Option<T>, tokio_postgres::Error>
    where
        T: for<'a> tokio_postgres::types::FromSql<'a>,
    {
        let stmt = client.prepare_cached(self.sql).await?;
        let row = client.as_client().query_opt(&stmt, &self.params).await?;
        Ok(row.as_ref().map(|r| r.get(0)))
    }

    /// Fetch the first column of every row into a `Vec`.
    #[inline]
    pub async fn fetch_all<T>(
        self,
        client: &impl GenericClient,
    ) -> Result<Vec<T>, tokio_postgres::Error>
    where
        T: for<'a> tokio_postgres::types::FromSql<'a>,
    {
        let stmt = client.prepare_cached(self.sql).await?;
        let rows = client.as_client().query(&stmt, &self.params).await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    /// Stream the first column of each row lazily.
    #[inline]
    pub async fn fetch<T>(
        self,
        client: &impl GenericClient,
    ) -> Result<
        impl futures_util::Stream<Item = Result<T, tokio_postgres::Error>>,
        tokio_postgres::Error,
    >
    where
        T: for<'a> tokio_postgres::types::FromSql<'a>,
    {
        let stmt = client.prepare_cached(self.sql).await?;
        let stream = client
            .as_client()
            .query_raw(&stmt, self.params.iter().copied())
            .await?;
        Ok(futures_util::TryStreamExt::map_ok(stream, |row| row.get(0)))
    }
}

// ─── Statement<'q, N> — zero-cost execute-only query builder ─────────────────

pub struct Statement<'q, const N: usize> {
    sql: &'static str,
    params: [&'q (dyn tokio_postgres::types::ToSql + Sync + 'q); N],
}

impl<'q, const N: usize> Statement<'q, N> {
    #[doc(hidden)]
    #[inline(always)]
    pub fn new(
        sql: &'static str,
        params: [&'q (dyn tokio_postgres::types::ToSql + Sync + 'q); N],
    ) -> Self {
        Self { sql, params }
    }

    /// Execute the statement and return the number of affected rows.
    #[inline]
    pub async fn execute(self, client: &impl GenericClient) -> Result<u64, tokio_postgres::Error> {
        let stmt = client.prepare_cached(self.sql).await?;
        client.as_client().execute(&stmt, &self.params).await
    }
}

// ─── Const assertion helpers ──────────────────────────────────────────────────

/// Byte-by-byte string equality, usable in `const` contexts.
pub const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Verify two column-name slices match element-by-element.
pub const fn columns_match(expected: &[&str], actual: &[&str]) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    let mut i = 0;
    while i < expected.len() {
        if !str_eq(expected[i], actual[i]) {
            return false;
        }
        i += 1;
    }
    true
}
