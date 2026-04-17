use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Expr, Fields, Lit, Meta};

pub fn derive_to_row(input: DeriveInput) -> TokenStream {
    let struct_name = &input.ident;

    // ── 1. Extract #[table = "..."] ─────────────────────────────────────────
    let table_name = input
        .attrs
        .iter()
        .find_map(|attr| {
            if !attr.path().is_ident("table") {
                return None;
            }
            if let Meta::NameValue(mnv) = &attr.meta {
                if let Expr::Lit(el) = &mnv.value {
                    if let Lit::Str(s) = &el.lit {
                        return Some(s.value());
                    }
                }
            }
            None
        })
        .unwrap_or_else(|| {
            panic!("ToRow: missing #[table = \"table_name\"] attribute on {struct_name}")
        });

    // ── 2. Parse fields ──────────────────────────────────────────────────────
    let named_fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => &f.named,
            _ => panic!("ToRow: only named fields supported"),
        },
        _ => panic!("ToRow: only structs supported"),
    };

    struct FieldDef {
        name: syn::Ident,
        is_option: bool,
        inner_type: String,
        /// Marked with `#[id]` — the Rust field type is a Uuid newtype that
        /// implements `service_core::id::Id`, and the collector must call
        /// `as_uuid()` to extract a raw `Uuid` for the `uuid[]` UNNEST slot.
        is_id: bool,
    }

    let fields: Vec<FieldDef> = named_fields
        .iter()
        .map(|f| {
            let name = f.ident.clone().unwrap();
            let is_id = f.attrs.iter().any(|a| a.path().is_ident("id"));
            if let Some(inner) = crate::extract_option_inner(&f.ty) {
                let inner_type = if is_id { "Uuid".into() } else { crate::type_name_string(&inner) };
                FieldDef { name, is_option: true, inner_type, is_id }
            } else {
                let inner_type = if is_id { "Uuid".into() } else { crate::type_name_string(&f.ty) };
                FieldDef {
                    name,
                    is_option: false,
                    inner_type,
                    is_id,
                }
            }
        })
        .collect();

    // ── 3. Build INSERT … UNNEST SQL ─────────────────────────────────────────
    let cols_str = fields.iter().map(|f| f.name.to_string()).collect::<Vec<_>>().join(", ");

    let unnest_parts: Vec<String> = fields
        .iter()
        .enumerate()
        .map(|(i, f)| format!("${}::{}", i + 1, pg_array_cast(&f.inner_type)))
        .collect();
    let unnest_str = unnest_parts.join(", ");

    let sql =
        format!("INSERT INTO {table_name} ({cols_str}) SELECT * FROM UNNEST({unnest_str})");

    // ── 4. Optional compile-time DB validation ───────────────────────────────
    if let Err(e) = validate_sql_against_db(&sql) {
        return syn::Error::new(proc_macro2::Span::call_site(), e).to_compile_error();
    }

    // ── 5. Generate column collection statements ─────────────────────────────
    let col_stmts: Vec<TokenStream> = fields
        .iter()
        .enumerate()
        .map(|(i, f)| gen_collect_stmt(format_ident!("__col_{i}"), &f.name, &f.inner_type, f.is_option, f.is_id))
        .collect();

    let param_refs: Vec<TokenStream> = (0..fields.len())
        .map(|i| {
            let var = format_ident!("__col_{i}");
            quote! { &#var as &(dyn ::tokio_postgres::types::ToSql + Sync) }
        })
        .collect();

    // ── 6. Emit impl ─────────────────────────────────────────────────────────
    quote! {
        impl from_row::ToRow for #struct_name {
            async fn batch_insert(
                rows: &[Self],
                client: &impl from_row::GenericClient,
            ) -> Result<u64, ::tokio_postgres::Error> {
                const SQL: &'static str = #sql;
                #(#col_stmts)*
                let __stmt = client.prepare_cached(SQL).await?;
                client.as_client().execute(&__stmt, &[#(#param_refs),*]).await
            }
        }
    }
}

// ── Type mapping ─────────────────────────────────────────────────────────────

fn pg_array_cast(type_name: &str) -> &str {
    match type_name {
        "String" => "text[]",
        "Uuid" => "uuid[]",
        "i16" => "int2[]",
        "i32" => "int4[]",
        "i64" => "int8[]",
        "f32" => "float4[]",
        "f64" => "float8[]",
        "bool" => "bool[]",
        "Timestamp" => "timestamptz[]",
        "DateTime" => "timestamp[]",
        "Date" => "date[]",
        "Time" => "time[]",
        "Decimal" => "numeric[]",
        t => panic!("ToRow: unsupported field type '{t}'"),
    }
}

fn gen_collect_stmt(
    var: syn::Ident,
    field: &syn::Ident,
    inner_type: &str,
    is_option: bool,
    is_id: bool,
) -> TokenStream {
    // Id newtype fields: extract via the `Id::as_uuid()` trait method so the
    // UNNEST slot receives a `Vec<Uuid>` / `Vec<Option<Uuid>>` regardless of
    // which specific newtype is wrapping it.
    if is_id {
        if is_option {
            return quote! {
                let #var: Vec<Option<::uuid::Uuid>> = rows
                    .iter()
                    .map(|r| r.#field.as_ref().map(|v| ::service_core::id::Id::as_uuid(v)))
                    .collect();
            };
        } else {
            return quote! {
                let #var: Vec<::uuid::Uuid> = rows
                    .iter()
                    .map(|r| ::service_core::id::Id::as_uuid(&r.#field))
                    .collect();
            };
        }
    }

    if is_option {
        match inner_type {
            "String" => quote! {
                let #var: Vec<Option<&str>> =
                    rows.iter().map(|r| r.#field.as_deref()).collect();
            },
            "Uuid" => quote! {
                let #var: Vec<Option<::uuid::Uuid>> =
                    rows.iter().map(|r| r.#field).collect();
            },
            "i16" => quote! {
                let #var: Vec<Option<i16>> = rows.iter().map(|r| r.#field).collect();
            },
            "i32" => quote! {
                let #var: Vec<Option<i32>> = rows.iter().map(|r| r.#field).collect();
            },
            "i64" => quote! {
                let #var: Vec<Option<i64>> = rows.iter().map(|r| r.#field).collect();
            },
            "f32" => quote! {
                let #var: Vec<Option<f32>> = rows.iter().map(|r| r.#field).collect();
            },
            "f64" => quote! {
                let #var: Vec<Option<f64>> = rows.iter().map(|r| r.#field).collect();
            },
            "bool" => quote! {
                let #var: Vec<Option<bool>> = rows.iter().map(|r| r.#field).collect();
            },
            "Timestamp" => quote! {
                let #var: Vec<Option<::jiff::Timestamp>> =
                    rows.iter().map(|r| r.#field).collect();
            },
            "DateTime" => quote! {
                let #var: Vec<Option<::jiff::civil::DateTime>> =
                    rows.iter().map(|r| r.#field).collect();
            },
            "Date" => quote! {
                let #var: Vec<Option<::jiff::civil::Date>> =
                    rows.iter().map(|r| r.#field).collect();
            },
            "Time" => quote! {
                let #var: Vec<Option<::jiff::civil::Time>> =
                    rows.iter().map(|r| r.#field).collect();
            },
            "Decimal" => quote! {
                let #var: Vec<Option<::rust_decimal::Decimal>> =
                    rows.iter().map(|r| r.#field).collect();
            },
            t => panic!("ToRow: unsupported Option<{t}> field type"),
        }
    } else {
        match inner_type {
            "String" => quote! {
                let #var: Vec<&str> = rows.iter().map(|r| r.#field.as_str()).collect();
            },
            "Uuid" => quote! {
                let #var: Vec<::uuid::Uuid> = rows.iter().map(|r| r.#field).collect();
            },
            "i16" => quote! {
                let #var: Vec<i16> = rows.iter().map(|r| r.#field).collect();
            },
            "i32" => quote! {
                let #var: Vec<i32> = rows.iter().map(|r| r.#field).collect();
            },
            "i64" => quote! {
                let #var: Vec<i64> = rows.iter().map(|r| r.#field).collect();
            },
            "f32" => quote! {
                let #var: Vec<f32> = rows.iter().map(|r| r.#field).collect();
            },
            "f64" => quote! {
                let #var: Vec<f64> = rows.iter().map(|r| r.#field).collect();
            },
            "bool" => quote! {
                let #var: Vec<bool> = rows.iter().map(|r| r.#field).collect();
            },
            "Timestamp" => quote! {
                let #var: Vec<::jiff::Timestamp> =
                    rows.iter().map(|r| r.#field).collect();
            },
            "DateTime" => quote! {
                let #var: Vec<::jiff::civil::DateTime> =
                    rows.iter().map(|r| r.#field).collect();
            },
            "Date" => quote! {
                let #var: Vec<::jiff::civil::Date> =
                    rows.iter().map(|r| r.#field).collect();
            },
            "Time" => quote! {
                let #var: Vec<::jiff::civil::Time> =
                    rows.iter().map(|r| r.#field).collect();
            },
            "Decimal" => quote! {
                let #var: Vec<::rust_decimal::Decimal> =
                    rows.iter().map(|r| r.#field).collect();
            },
            t => panic!("ToRow: unsupported field type '{t}'"),
        }
    }
}

// ── Compile-time DB validation ────────────────────────────────────────────────

/// Prepare the INSERT SQL against the live DB. Returns Ok if unreachable (silently skip).
fn validate_sql_against_db(sql: &str) -> Result<(), String> {
    dotenvy::dotenv().ok();
    let url = match std::env::var("DATABASE_URL") {
        Ok(u) => u,
        Err(_) => return Ok(()),
    };
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(_) => return Ok(()),
    };
    rt.block_on(async {
        let (client, conn) = match tokio_postgres::connect(&url, tokio_postgres::NoTls).await {
            Ok(pair) => pair,
            Err(_) => return Ok(()),
        };
        tokio::spawn(async move { let _ = conn.await; });
        client.prepare(sql).await.map(|_| ()).map_err(|e| e.to_string())
    })
}
