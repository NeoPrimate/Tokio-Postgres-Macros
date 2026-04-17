use heck::ToUpperCamelCase;

/// Maps a PostgreSQL `udt_name` to the expected Rust type last-segment,
/// matching what `from_row_derive` emits into `COLUMN_TYPES`.
///
/// For known built-in types, returns the standard Rust type name.
/// For unknown types (custom PG enums like `order_status`), infers the
/// Rust type name by converting snake_case to PascalCase.
pub fn expected_rust_type(udt_name: &str) -> String {
    match udt_name {
        "uuid" => "Uuid".into(),
        "text" | "varchar" | "bpchar" => "String".into(),
        "int2" => "i16".into(),
        "int4" => "i32".into(),
        "int8" => "i64".into(),
        "float4" => "f32".into(),
        "float8" => "f64".into(),
        "numeric" => "Decimal".into(),
        "bool" => "bool".into(),
        "timestamptz" => "Timestamp".into(),
        "timestamp" => "DateTime".into(),
        "date" => "Date".into(),
        "time" | "timetz" => "Time".into(),
        "json" | "jsonb" => "Value".into(),
        "bytea" => "Vec".into(),
        // Custom PG enums: "order_status" → "OrderStatus", "governance_action" → "GovernanceAction"
        other => other.to_upper_camel_case(),
    }
}

/// Full Rust type path for a bind parameter assertion.
/// Used in the `if false` type-checking block (sqlx-style WrapSame + MatchBorrow).
/// Returns `""` for custom PG enum types, arrays, etc. — assertion is skipped
/// (we can't generate a type path without knowing the module).
pub fn bind_rust_type(udt_name: &str) -> &'static str {
    match udt_name {
        "uuid" => "::uuid::Uuid",
        "int2" => "i16",
        "int4" => "i32",
        "int8" => "i64",
        "float4" => "f32",
        "float8" => "f64",
        "numeric" => "::rust_decimal::Decimal",
        "bool" => "bool",
        "timestamptz" => "::jiff::Timestamp",
        "timestamp" => "::jiff::civil::DateTime",
        "date" => "::jiff::civil::Date",
        "time" | "timetz" => "::jiff::civil::Time",
        "text" | "varchar" | "bpchar" => "String",
        "json" | "jsonb" => "::serde_json::Value",
        "bytea" => "Vec<u8>",
        _ => "",  // custom PG enums — can't infer module path
    }
}
