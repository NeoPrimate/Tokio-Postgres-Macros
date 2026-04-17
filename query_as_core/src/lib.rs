//! Process-wide Tokio runtime and per-database PostgreSQL clients shared
//! across every `query_as!` / `query_scalar!` / `query!` / `#[derive(ToRow)]`
//! macro invocation in a single compiler run.
//!
//! Rationale: proc-macros were previously spawning a new `Runtime` + TCP
//! connection *per macro* — O(queries) TCP handshakes dominate incremental
//! build time. This module caches:
//!
//! * one [`tokio::runtime::Runtime`] (multi-threaded, default),
//! * a `HashMap<URL, Client>` so a single `cargo check --workspace`
//!   invocation that touches multiple service databases gets at most one
//!   connection per database,
//! * a per-URL schema cache keyed by table name so `pg_class` /
//!   `pg_attribute` introspection is done at most once per table per
//!   `cargo` invocation.
//!
//! All access is serialized through a `Mutex` — rustc can parallelize
//! proc-macro expansion across crates/threads, so concurrent access is
//! real. The mutex makes the proc-macro side strictly sequential, but the
//! cost is trivial compared to even a single DB round trip.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tokio::runtime::Runtime;
use tokio_postgres::{Client, NoTls, Statement};

/// Table schema: one row per (non-dropped, `attnum > 0`) column in
/// `pg_attribute`.
#[derive(Clone, Debug)]
pub struct ColumnInfo {
    pub table_oid: u32,
    pub attnum: i16,
    pub name: String,
    pub attnotnull: bool,
}

/// Per-database state.
struct DbState {
    client: Client,
    /// `table_name` → rows ordered by `attnum` ascending.
    schema_cache: HashMap<String, Vec<ColumnInfo>>,
}

/// Shared state across all proc-macro invocations in the current compiler
/// process.
struct Shared {
    rt: Runtime,
    /// `DATABASE_URL` → per-database client + schema cache. One entry per
    /// distinct URL seen during this cargo invocation.
    dbs: HashMap<String, DbState>,
    /// Whether we've already printed the "DATABASE_URL not set" warning —
    /// keeps the build output tidy.
    warned_no_url: bool,
}

/// * `Uninit` — first caller will try to initialize the runtime.
/// * `Ready(shared)` — runtime exists, clients populated on demand.
///
/// Offline handling (`QUERY_AS_OFFLINE=1`, or no `DATABASE_URL`) is
/// done at `with_state` call time, so there's no dedicated state for it.
enum Slot {
    Uninit,
    Ready(Shared),
}

static STATE: OnceLock<Mutex<Slot>> = OnceLock::new();

pub type ConnError = String;

fn slot() -> &'static Mutex<Slot> {
    STATE.get_or_init(|| Mutex::new(Slot::Uninit))
}

/// Load `.env` from the calling crate's manifest dir (`CARGO_MANIFEST_DIR`)
/// and then from the process CWD, without overriding variables that are
/// already set in the environment. Mirrors `dotenvy::dotenv` semantics.
fn load_env() {
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let env_path = std::path::Path::new(&manifest_dir).join(".env");
        if env_path.exists() {
            dotenvy::from_path(&env_path).ok();
        }
    }
    dotenvy::dotenv().ok();
}

/// Return the URL the *current* proc-macro invocation should validate
/// against. Looks at the ambient environment after `.env` loading. Returns
/// `None` if there's no URL (soft-offline).
fn current_url() -> Option<String> {
    load_env();
    match std::env::var("DATABASE_URL") {
        Ok(u) if !u.is_empty() => Some(u),
        _ => None,
    }
}

/// Run `f` with exclusive access to the per-URL DB state. The URL is
/// resolved from `DATABASE_URL` + `.env` at call time, so each proc-macro
/// gets the connection appropriate for its own crate.
///
/// * `Ok(None)` — offline (explicit `QUERY_AS_OFFLINE=1`, or no URL).
/// * `Ok(Some(r))` — validation ran and produced `r`.
/// * `Err(msg)` — initialization or callback failure; surfaces as a
///   compile error.
pub fn with_state<F, R>(f: F) -> Result<Option<R>, ConnError>
where
    F: FnOnce(&mut ConnStateHandle<'_>) -> Result<R, ConnError>,
{
    // Explicit offline check before touching the mutex.
    if std::env::var("QUERY_AS_OFFLINE").ok().as_deref() == Some("1") {
        return Ok(None);
    }

    let mu = slot();
    let mut guard = mu
        .lock()
        .map_err(|_| "shared conn mutex poisoned".to_string())?;

    // Lazy runtime creation.
    if matches!(&*guard, Slot::Uninit) {
        *guard = Slot::Ready(Shared {
            rt: Runtime::new().map_err(|e| format!("proc-macro runtime: {e}"))?,
            dbs: HashMap::new(),
            warned_no_url: false,
        });
    }

    let shared = match &mut *guard {
        Slot::Ready(s) => s,
        Slot::Uninit => unreachable!("just initialized above"),
    };

    // Resolve the URL from this call's environment.
    let url = match current_url() {
        Some(u) => u,
        None => {
            if !shared.warned_no_url {
                shared.warned_no_url = true;
                eprintln!(
                    "warning: query_as_core: DATABASE_URL not set — skipping \
                     compile-time SQL validation. Set DATABASE_URL in a .env, \
                     or set QUERY_AS_OFFLINE=1 to silence this warning."
                );
            }
            return Ok(None);
        }
    };

    // Ensure a client for this URL.
    if !shared.dbs.contains_key(&url) {
        let client = shared
            .rt
            .block_on(async {
                let (client, conn) = tokio_postgres::connect(&url, NoTls)
                    .await
                    .map_err(|e| format!("connect({url}): {e}"))?;
                tokio::spawn(async move {
                    let _ = conn.await;
                });
                Ok::<_, String>(client)
            })?;
        shared.dbs.insert(
            url.clone(),
            DbState {
                client,
                schema_cache: HashMap::new(),
            },
        );
    }
    let db = shared.dbs.get_mut(&url).expect("just inserted");

    let mut handle = ConnStateHandle { rt: &shared.rt, db };
    let r = f(&mut handle)?;
    Ok(Some(r))
}

/// Prepare `sql` against the shared live DB without caring about its output
/// columns. Used by `#[derive(ToRow)]` to validate its generated INSERT SQL.
///
/// * Returns `Ok(())` in offline mode.
/// * Returns `Ok(())` if the target table does not exist — `ToRow` is
///   derived inside the `models` crate which doesn't know which service DB
///   a given struct belongs to, so a `cargo check --workspace` pointed at
///   one service DB will see "relation does not exist" for every other
///   service's tables. We skip these. Real bugs (wrong column type,
///   syntax error) still surface because they produce different SQL states.
/// * Returns `Err(msg)` on any other failure.
pub fn validate_sql(sql: &str) -> Result<(), ConnError> {
    with_state(|state| match state.prepare(sql) {
        Ok(_) => Ok(()),
        Err(e) if is_undefined_table(&e) => Ok(()),
        Err(e) => Err(e),
    })
    .map(|_| ())
}

fn is_undefined_table(err: &str) -> bool {
    err.contains("does not exist") && err.contains("relation")
}

/// Thin façade the callers use — avoids leaking `Shared` internals.
pub struct ConnStateHandle<'a> {
    rt: &'a Runtime,
    db: &'a mut DbState,
}

impl ConnStateHandle<'_> {
    /// Prepare `sql` against the live DB.
    pub fn prepare(&mut self, sql: &str) -> Result<Statement, ConnError> {
        let client = &self.db.client;
        self.rt
            .block_on(async { client.prepare(sql).await })
            .map_err(|e| {
                // `tokio_postgres::Error::Display` is terse ("db error").
                // The `DbError` source carries the useful payload
                // (SQLSTATE, message, position). Walk the error chain to
                // get something actionable into the compile output.
                use std::error::Error;
                let mut src: Option<&dyn Error> = Some(&e as &dyn Error);
                let mut detail = String::new();
                while let Some(s) = src {
                    if !detail.is_empty() {
                        detail.push_str(": ");
                    }
                    detail.push_str(&s.to_string());
                    src = s.source();
                }
                format!("prepare: {detail}")
            })
    }

    /// Return schema rows for `tables`. Tables already in the cache are
    /// returned from memory; the rest are fetched in a single batch query.
    pub fn load_schema(
        &mut self,
        tables: &[&str],
    ) -> Result<HashMap<String, Vec<ColumnInfo>>, ConnError> {
        let mut out: HashMap<String, Vec<ColumnInfo>> = HashMap::new();
        let mut missing: Vec<&str> = Vec::new();
        for t in tables {
            if let Some(rows) = self.db.schema_cache.get(*t) {
                out.insert((*t).to_string(), rows.clone());
            } else {
                missing.push(*t);
            }
        }

        if !missing.is_empty() {
            let client = &self.db.client;
            let rt = self.rt;
            let rows = rt
                .block_on(async {
                    client
                        .query(
                            "SELECT c.relname, c.oid::bigint, a.attname, a.attnum, a.attnotnull \
                             FROM pg_class c \
                             JOIN pg_attribute a ON a.attrelid = c.oid \
                             WHERE c.relname = ANY($1) \
                               AND c.relnamespace = \
                                   (SELECT oid FROM pg_namespace WHERE nspname = 'public') \
                               AND a.attnum > 0 \
                               AND NOT a.attisdropped \
                             ORDER BY c.relname, a.attnum",
                            &[&missing],
                        )
                        .await
                })
                .map_err(|e| format!("schema introspection: {e}"))?;

            let mut grouped: HashMap<String, Vec<ColumnInfo>> = HashMap::new();
            // Pre-seed empty vectors so tables with zero matching rows are
            // still cached (prevents re-querying them on every invocation).
            for t in &missing {
                grouped.entry((*t).to_string()).or_default();
            }
            for row in &rows {
                let tname: &str = row.get(0);
                let oid: i64 = row.get(1);
                let name: &str = row.get(2);
                let attnum: i16 = row.get(3);
                let attnotnull: bool = row.get(4);
                grouped
                    .entry(tname.to_owned())
                    .or_default()
                    .push(ColumnInfo {
                        table_oid: oid as u32,
                        attnum,
                        name: name.to_owned(),
                        attnotnull,
                    });
            }
            for (tname, cols) in grouped {
                self.db.schema_cache.insert(tname.clone(), cols.clone());
                out.insert(tname, cols);
            }
        }

        Ok(out)
    }
}
