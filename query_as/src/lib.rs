mod db;
mod sql;
mod types;

use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{Expr, Ident, LitStr, Token};

// ─── query_as! input ─────────────────────────────────────────────────────────

/// `query_as!(StructName, "SQL", bind1, bind2, ...)`
struct QueryAsInput {
    struct_name: Ident,
    sql: LitStr,
    binds: Vec<Expr>,
}

impl Parse for QueryAsInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let struct_name: Ident = input.parse()?;
        input.parse::<Token![,]>()?;
        let sql: LitStr = input.parse()?;

        let mut binds = Vec::new();
        while input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            if !input.is_empty() {
                binds.push(input.parse()?);
            }
        }

        Ok(QueryAsInput { struct_name, sql, binds })
    }
}

// ─── query_scalar! input ─────────────────────────────────────────────────────

/// `query_scalar!("SQL", bind1, bind2, ...)`
struct ScalarInput {
    sql: LitStr,
    binds: Vec<Expr>,
}

impl Parse for ScalarInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let sql: LitStr = input.parse()?;

        let mut binds = Vec::new();
        while input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            if !input.is_empty() {
                binds.push(input.parse()?);
            }
        }

        Ok(ScalarInput { sql, binds })
    }
}

// ─── Proc macros ─────────────────────────────────────────────────────────────

/// `query_as!(Struct, "SQL", bind1, ...)` → `Query<'_, Struct, N>`
///
/// Chain with `.fetch_one(&client)`, `.fetch_opt(&client)`, `.fetch_all(&client)`,
/// or `.fetch(&client)` (streaming). All validated at compile time against the DB.
#[proc_macro]
pub fn query_as(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as QueryAsInput);
    match expand_query(&input) {
        Ok(tokens) => tokens.into(),
        Err(msg) => syn::Error::new_spanned(&input.sql, msg).to_compile_error().into(),
    }
}

/// `query_scalar!("SQL", bind1, ...)` → `Scalar<'_, N>`
///
/// Chain with `.fetch_one::<T>(&client)`, `.fetch_opt::<T>(&client)`,
/// `.fetch_all::<T>(&client)`, or `.fetch::<T>(&client)`. Bind types checked at compile time.
#[proc_macro]
pub fn query_scalar(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as ScalarInput);
    match expand_scalar(&input) {
        Ok(tokens) => tokens.into(),
        Err(msg) => syn::Error::new_spanned(&input.sql, msg).to_compile_error().into(),
    }
}

/// `query!("SQL", bind1, ...)` → `Statement<'_, N>`
///
/// Chain with `.execute(&client)` → `Result<u64, Error>`.
/// For statements with no row output (INSERT without RETURNING, UPDATE, DELETE).
/// Bind types checked at compile time.
#[proc_macro]
pub fn query(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as ScalarInput);
    match expand_statement(&input) {
        Ok(tokens) => tokens.into(),
        Err(msg) => syn::Error::new_spanned(&input.sql, msg).to_compile_error().into(),
    }
}

// ─── Expansion ───────────────────────────────────────────────────────────────

fn expand_query(input: &QueryAsInput) -> Result<proc_macro2::TokenStream, String> {
    let sql_str = input.sql.value();
    let analysis = sql::analyze(&sql_str)?;
    let db_result = db::analyze(&sql_str, &analysis);

    if !db_result.errors.is_empty() {
        return Err(db_result.errors.join("\n"));
    }

    let struct_name = &input.struct_name;
    let sql_lit = &input.sql;
    let binds = &input.binds;
    let n = binds.len();

    let sql_col_names: Vec<&str> = db_result
        .output_cols
        .iter()
        .map(|(name, _, _)| name.as_str())
        .collect();

    let type_assertions: Vec<proc_macro2::TokenStream> = db_result
        .output_cols
        .iter()
        .enumerate()
        .filter_map(|(i, (col_name, udt_name, nullable))| {
            let expected = types::expected_rust_type(udt_name.as_str());
            if expected.is_empty() {
                return None;
            }
            let type_err = format!(
                "Type mismatch for column '{col_name}': \
                 SQL type '{udt_name}' expects Rust type '{expected}'"
            );
            let type_assert = quote! {
                assert!(
                    from_row::str_eq(#struct_name::COLUMN_TYPES[#i].1, #expected),
                    #type_err
                );
            };
            let null_assert = if *nullable {
                let null_err = format!(
                    "Nullability mismatch for column '{col_name}': \
                     SQL column is nullable but struct field is not Option<_>"
                );
                quote! { assert!(#struct_name::COLUMN_TYPES[#i].2, #null_err); }
            } else {
                quote! {}
            };
            Some(quote! { #type_assert #null_assert })
        })
        .collect();

    let bind_type_assertions = bind_assertions(binds, &db_result.bind_types);

    Ok(quote! {
        {
            const _: () = {
                const SQL_COLS: &[&str] = &[#(#sql_col_names),*];
                assert!(
                    from_row::columns_match(#struct_name::EXPECTED_COLUMNS, SQL_COLS),
                    "SQL output columns do not match struct fields"
                );
                #(#type_assertions)*
            };

            #(#bind_type_assertions)*

            from_row::Query::<#struct_name, #n>::new(
                #sql_lit,
                [#(&(#binds) as &(dyn ::tokio_postgres::types::ToSql + Sync)),*],
            )
        }
    })
}

fn expand_scalar(input: &ScalarInput) -> Result<proc_macro2::TokenStream, String> {
    let sql_str = input.sql.value();
    let sql_lit = &input.sql;
    let binds = &input.binds;
    let n = binds.len();

    let db_bind_types = db::bind_types(&sql_str)?;
    let bind_type_assertions = bind_assertions(binds, &db_bind_types);

    Ok(quote! {
        {
            #(#bind_type_assertions)*

            from_row::Scalar::<#n>::new(
                #sql_lit,
                [#(&(#binds) as &(dyn ::tokio_postgres::types::ToSql + Sync)),*],
            )
        }
    })
}

fn expand_statement(input: &ScalarInput) -> Result<proc_macro2::TokenStream, String> {
    let sql_str = input.sql.value();
    let sql_lit = &input.sql;
    let binds = &input.binds;
    let n = binds.len();

    let db_bind_types = db::bind_types(&sql_str)?;
    let bind_type_assertions = bind_assertions(binds, &db_bind_types);

    Ok(quote! {
        {
            #(#bind_type_assertions)*

            from_row::Statement::<#n>::new(
                #sql_lit,
                [#(&(#binds) as &(dyn ::tokio_postgres::types::ToSql + Sync)),*],
            )
        }
    })
}

// ─── Shared helpers ───────────────────────────────────────────────────────────

fn bind_assertions(
    binds: &[Expr],
    db_bind_types: &[Option<String>],
) -> Vec<proc_macro2::TokenStream> {
    binds
        .iter()
        .enumerate()
        .filter_map(|(i, expr)| {
            let pg_type = db_bind_types.get(i)?.as_ref()?;
            let rust_type_str = types::bind_rust_type(pg_type);
            if rust_type_str.is_empty() {
                return None; // custom PG enum or unknown type — skip
            }

            // UUID bindings take a different path: we check against the
            // `UuidBind` marker trait rather than requiring type equality with
            // `::uuid::Uuid`. This lets id newtypes (e.g. `ProductId(Uuid)`)
            // that derive `Id` flow through as uuid parameters while still
            // rejecting unrelated types (`bool`, `String`, `i32`, `Decimal`,
            // …) at compile time — they don't implement `UuidBind`.
            if pg_type == "uuid" {
                let arg_name = quote::format_ident!("_arg{}", i);
                return Some(quote::quote_spanned!(expr.span() =>
                    let #arg_name = &(#expr);
                    #[allow(clippy::unreachable, unused)]
                    if false {
                        fn _assert_uuid_bind<T: ::from_row::UuidBind + ?Sized>(_: &T) {}
                        _assert_uuid_bind(#arg_name);
                        ::core::unreachable!();
                    }
                ));
            }

            let param_ty: syn::Type = syn::parse_str(rust_type_str).ok()?;
            let arg_name = quote::format_ident!("_arg{}", i);

            Some(quote::quote_spanned!(expr.span() =>
                let #arg_name = &(#expr);
                #[allow(clippy::unreachable, unused)]
                if false {
                    use from_row::ty_match::{WrapSameExt as _, MatchBorrowExt as _};
                    let _expr = from_row::ty_match::dupe_value(#arg_name);
                    let _ty_check = from_row::ty_match::WrapSame::<#param_ty, _>::new(&_expr).wrap_same();
                    let (mut _ty_check, _match_borrow) = from_row::ty_match::MatchBorrow::new(_ty_check, &_expr);
                    _ty_check = _match_borrow.match_borrow();
                    ::core::unreachable!();
                }
            ))
        })
        .collect()
}
