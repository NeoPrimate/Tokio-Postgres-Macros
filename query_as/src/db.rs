use crate::sql::{JoinKind, SqlAnalysis};

pub struct DbAnalysis {
    /// (output_alias, udt_name, nullable) for each SELECT column, in order.
    pub output_cols: Vec<(String, String, bool)>,
    /// Per $N bind param (0-indexed): PostgreSQL type name if determinable.
    pub bind_types: Vec<Option<String>>,
    /// Validation errors to surface as compile errors.
    pub errors: Vec<String>,
}

impl DbAnalysis {
    fn empty() -> Self {
        Self { output_cols: vec![], bind_types: vec![], errors: vec![] }
    }
}

/// Connect to the DB, prepare the SQL, and return full analysis.
/// Returns an empty `DbAnalysis` (no errors, no assertions) if the DB is unreachable.
pub fn analyze(sql: &str, analysis: &SqlAnalysis) -> DbAnalysis {
    // Load .env from the calling crate's directory (CARGO_MANIFEST_DIR),
    // not the workspace root CWD. This ensures each service crate
    // validates against its own database.
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let env_path = std::path::Path::new(&manifest_dir).join(".env");
        if env_path.exists() {
            dotenvy::from_path(&env_path).ok();
        }
    }
    dotenvy::dotenv().ok(); // fallback to CWD .env

    let url = match std::env::var("DATABASE_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            let mut r = DbAnalysis::empty();
            r.errors.push("DATABASE_URL not set. Each service crate needs a .env file with DATABASE_URL pointing to its database. Run ./init.sh to generate them.".to_string());
            return r;
        }
    };

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            let mut r = DbAnalysis::empty();
            r.errors.push(format!("Failed to create tokio runtime: {e}"));
            return r;
        }
    };

    rt.block_on(async {
        match analyze_async(sql, analysis, &url).await {
            Ok(r) => r,
            Err(e) => {
                let mut r = DbAnalysis::empty();
                r.errors.push(format!("{e}"));
                r
            }
        }
    })
}

async fn analyze_async(
    sql: &str,
    analysis: &SqlAnalysis,
    url: &str,
) -> Result<DbAnalysis, Box<dyn std::error::Error>> {
    let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls).await?;
    tokio::spawn(async move { let _ = conn.await; });

    // Prepare validates SQL against the live schema and gives us output + param types.
    let stmt = client.prepare(sql).await?;

    let bind_types: Vec<Option<String>> = stmt
        .params()
        .iter()
        .map(|ty| Some(ty.name().to_owned()))
        .collect();

    // Fetch schema info for all tables referenced in the query.
    let table_names: Vec<&str> = analysis.table_refs.iter()
        .map(|r| r.real_name.as_str())
        .collect();

    let schema_rows = if !table_names.is_empty() {
        client
            .query(
                "SELECT c.relname, c.oid::bigint, a.attname, a.attnum, a.attnotnull \
                 FROM pg_class c \
                 JOIN pg_attribute a ON a.attrelid = c.oid \
                 WHERE c.relname = ANY($1) \
                   AND c.relnamespace = \
                       (SELECT oid FROM pg_namespace WHERE nspname = 'public') \
                   AND a.attnum > 0 \
                   AND NOT a.attisdropped",
                &[&table_names],
            )
            .await?
    } else {
        vec![]
    };

    // Build lookup maps from the schema rows.
    let mut table_oid: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    // (table_oid, attnum) → (table_name, col_name, attnotnull)
    let mut col_map: std::collections::HashMap<(u32, i16), (String, String, bool)> =
        std::collections::HashMap::new();
    // (table_name, col_name) → (table_oid, attnum)
    let mut name_to_key: std::collections::HashMap<(String, String), (u32, i16)> =
        std::collections::HashMap::new();

    for row in &schema_rows {
        let tname: &str = row.get(0);
        let oid: u32 = row.get::<_, i64>(1) as u32;
        let cname: &str = row.get(2);
        let attnum: i16 = row.get(3);
        let attnotnull: bool = row.get(4);
        table_oid.insert(tname.to_owned(), oid);
        col_map.insert((oid, attnum), (tname.to_owned(), cname.to_owned(), attnotnull));
        name_to_key.insert((tname.to_owned(), cname.to_owned()), (oid, attnum));
    }

    // OIDs of LEFT JOINed tables — their columns are nullable regardless of attnotnull.
    let left_joined_oids: std::collections::HashSet<u32> = analysis
        .table_refs
        .iter()
        .filter(|r| r.join_kind == JoinKind::Left)
        .filter_map(|r| table_oid.get(&r.real_name).copied())
        .collect();

    // Join key columns are exempt from the NOT NULL completeness check.
    let join_key_exempt: std::collections::HashSet<(u32, i16)> = analysis
        .join_condition_columns
        .iter()
        .filter_map(|(alias, col_name)| {
            let real_name = &analysis.table_refs.iter().find(|r| r.alias == *alias)?.real_name;
            name_to_key.get(&(real_name.clone(), col_name.clone())).copied()
        })
        .collect();

    // Columns actually present in the SELECT output.
    let selected_set: std::collections::HashSet<(u32, i16)> = stmt
        .columns()
        .iter()
        .filter_map(|c| Some((c.table_oid()?, c.column_id()?)))
        .collect();

    // NOT NULL completeness: every NOT NULL column of every referenced table must
    // appear in the SELECT (unless it's a join key).
    let mut errors = Vec::new();
    for row in &schema_rows {
        let tname: &str = row.get(0);
        let oid: u32 = row.get::<_, i64>(1) as u32;
        let cname: &str = row.get(2);
        let attnum: i16 = row.get(3);
        let attnotnull: bool = row.get(4);
        let key = (oid, attnum);
        if attnotnull && !selected_set.contains(&key) && !join_key_exempt.contains(&key) {
            errors.push(format!(
                "Column '{tname}.{cname}' is NOT NULL but is not included in the SELECT. \
                 Add it to your query and struct, or use a different struct for a partial select."
            ));
        }
    }

    // Build output columns with correct nullability.
    let output_cols = stmt
        .columns()
        .iter()
        .map(|col| {
            let name = col.name().to_owned();
            let type_name = col.type_().name().to_owned();
            let nullable = match (col.table_oid(), col.column_id()) {
                (Some(oid), Some(attnum)) => {
                    let is_left_joined = left_joined_oids.contains(&oid);
                    let is_not_null = col_map.get(&(oid, attnum))
                        .map(|(_, _, nn)| *nn)
                        .unwrap_or(false);
                    !is_not_null || is_left_joined
                }
                // Expression column — nullability unknown, conservatively not nullable.
                _ => false,
            };
            (name, type_name, nullable)
        })
        .collect();

    Ok(DbAnalysis { output_cols, bind_types, errors })
}

/// Return bind parameter types for `sql` without any column completeness checks.
/// Used by scalar query macros where there is no struct to validate against.
/// Returns errors instead of silently swallowing them.
pub fn bind_types(sql: &str) -> Result<Vec<Option<String>>, String> {
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let env_path = std::path::Path::new(&manifest_dir).join(".env");
        if env_path.exists() {
            dotenvy::from_path(&env_path).ok();
        }
    }
    dotenvy::dotenv().ok();

    let url = match std::env::var("DATABASE_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => return Err("DATABASE_URL not set. Each service crate needs a .env file with DATABASE_URL. Run ./init.sh to generate them.".into()),
    };

    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| format!("Failed to create tokio runtime: {e}"))?;

    rt.block_on(async {
        let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .map_err(|e| format!("db error: {e}"))?;
        tokio::spawn(async move { let _ = conn.await; });
        let stmt = client.prepare(sql)
            .await
            .map_err(|e| format!("db error: {e}"))?;
        Ok(stmt.params().iter().map(|ty| Some(ty.name().to_owned())).collect())
    })
}
