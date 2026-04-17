mod to_row;

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, Data, DeriveInput, Fields, GenericArgument, PathArguments, Type};

// ─── Field classification ───────────────────────────────────────────────

enum FieldKind {
    Plain { rust_type: String },
    Nested { inner_ty: syn::Ident },
    OptionalNested { inner_ty: syn::Ident },
}

struct ParsedField {
    name: syn::Ident,
    kind: FieldKind,
    /// True if the original Rust type is Option<T>.
    is_optional: bool,
}

fn parse_fields(data: &Data) -> Vec<ParsedField> {
    let fields = match data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => &f.named,
            _ => panic!("FromRow: only named fields supported"),
        },
        _ => panic!("FromRow: only structs supported"),
    };

    fields
        .iter()
        .map(|f| {
            let name = f.ident.clone().unwrap();
            let is_nested = f.attrs.iter().any(|a| a.path().is_ident("nested"));
            // `#[id]` marks a field as a Uuid-newtype id. The Rust field type
            // is a newtype like `ProductId`, but the column is `uuid`, and
            // `query_as!`'s output-column assertion compares against the
            // string "Uuid". Honor the attribute by recording "Uuid" as the
            // rust_type instead of the newtype's own name.
            let is_id = f.attrs.iter().any(|a| a.path().is_ident("id"));

            let kind = if is_nested {
                if let Some(inner) = extract_option_inner(&f.ty) {
                    FieldKind::OptionalNested {
                        inner_ty: type_ident(&inner),
                    }
                } else {
                    FieldKind::Nested {
                        inner_ty: type_ident(&f.ty),
                    }
                }
            } else if is_id {
                FieldKind::Plain {
                    rust_type: "Uuid".into(),
                }
            } else {
                FieldKind::Plain {
                    rust_type: type_name_string(&f.ty),
                }
            };

            let is_optional = extract_option_inner(&f.ty).is_some()
                || matches!(&kind, FieldKind::OptionalNested { .. });

            ParsedField { name, kind, is_optional }
        })
        .collect()
}

fn extract_option_inner(ty: &Type) -> Option<Type> {
    if let Type::Path(tp) = ty {
        let seg = tp.path.segments.last()?;
        if seg.ident == "Option" {
            if let PathArguments::AngleBracketed(ab) = &seg.arguments {
                if let Some(GenericArgument::Type(inner)) = ab.args.first() {
                    return Some(inner.clone());
                }
            }
        }
    }
    None
}

fn type_ident(ty: &Type) -> syn::Ident {
    if let Type::Path(tp) = ty {
        tp.path.segments.last().unwrap().ident.clone()
    } else {
        panic!("FromRow: expected path type for nested field")
    }
}

fn type_name_string(ty: &Type) -> String {
    // Unwrap Option<T> to get the inner type name.
    // The is_nullable flag in COLUMN_TYPES already tracks Option-ness.
    if let Some(inner) = extract_option_inner(ty) {
        return type_name_string(&inner);
    }
    if let Type::Path(tp) = ty {
        tp.path.segments.last().unwrap().ident.to_string()
    } else {
        panic!("FromRow: unsupported field type")
    }
}

fn nested_type_ident(f: &ParsedField) -> &syn::Ident {
    match &f.kind {
        FieldKind::Nested { inner_ty } | FieldKind::OptionalNested { inner_ty } => inner_ty,
        _ => unreachable!(),
    }
}

fn is_optional_nested(f: &ParsedField) -> bool {
    matches!(f.kind, FieldKind::OptionalNested { .. })
}

// ─── Main entry points ──────────────────────────────────────────────────

#[proc_macro_derive(ToRow, attributes(table, id))]
pub fn derive_to_row(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    to_row::derive_to_row(input).into()
}

#[proc_macro_derive(FromRow, attributes(nested, id))]
pub fn derive_from_row(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let struct_name = &input.ident;
    let fields = parse_fields(&input.data);

    let plain: Vec<&ParsedField> = fields
        .iter()
        .filter(|f| matches!(f.kind, FieldKind::Plain { .. }))
        .collect();
    let nested: Vec<&ParsedField> = fields
        .iter()
        .filter(|f| !matches!(f.kind, FieldKind::Plain { .. }))
        .collect();

    let from_row_macro = gen_from_row_macro(struct_name, &fields);
    let columns_macro = gen_columns_macro(struct_name, &plain, &nested);
    let types_macro = gen_column_types_macro(struct_name, &plain, &nested);
    let impl_block = gen_impl_block(struct_name, &fields);

    quote! {
        #from_row_macro
        #columns_macro
        #types_macro
        #impl_block
    }
    .into()
}

// ─── __from_row_StructName! ─────────────────────────────────────────────

fn gen_from_row_macro(
    struct_name: &syn::Ident,
    fields: &[ParsedField],
) -> proc_macro2::TokenStream {
    let macro_name = format_ident!("__from_row_{}", struct_name);

    // Track column offset for each field
    let mut offset = 0usize;
    let field_exprs: Vec<_> = fields
        .iter()
        .map(|f| {
            let fname = &f.name;
            let current_offset = offset;
            match &f.kind {
                FieldKind::Plain { .. } => {
                    offset += 1;
                    quote! { #fname: $row.get($start + #current_offset) }
                }
                FieldKind::Nested { inner_ty } => {
                    let nested_macro = format_ident!("__from_row_{}", inner_ty);
                    let col_count_const = format_ident!("__col_count_{}", inner_ty);
                    // Nested struct consumes its column count
                    let expr = quote! { #fname: #nested_macro!($row, $start + #current_offset) };
                    // Advance offset by nested struct's column count (known at compile time via const)
                    // We use a placeholder that will be resolved by the nested struct's EXPECTED_COLUMNS.len()
                    offset += 1; // Will be corrected below
                    expr
                }
                FieldKind::OptionalNested { inner_ty } => {
                    let nested_macro = format_ident!("__from_row_{}", inner_ty);
                    let expr = quote! {
                        #fname: if $row.is_null($start + #current_offset) {
                            None
                        } else {
                            Some(#nested_macro!($row, $start + #current_offset))
                        }
                    };
                    offset += 1; // Will be corrected below
                    expr
                }
            }
        })
        .collect();

    // For nested structs, we need to know their column count at compile time.
    // Since EXPECTED_COLUMNS is a const, we can compute this.
    // However, macro_rules! can't do arithmetic with const values directly.
    // Solution: use a simpler approach — count columns from the fields we know.
    // Re-compute offsets correctly knowing nested struct field counts.
    let mut offset2 = 0usize;
    let field_exprs2: Vec<_> = fields
        .iter()
        .map(|f| {
            let fname = &f.name;
            let current_offset = offset2;
            match &f.kind {
                FieldKind::Plain { .. } => {
                    offset2 += 1;
                    quote! { #fname: $row.get($start + #current_offset) }
                }
                FieldKind::Nested { inner_ty } => {
                    let nested_macro = format_ident!("__from_row_{}", inner_ty);
                    let expr = quote! { #fname: #nested_macro!($row, $start + #current_offset) };
                    // Nested struct's column count: we need to know it at macro-generation time.
                    // We have access to `inner_ty` but not its field count here.
                    // Use EXPECTED_COLUMNS.len() via a const trick in the generated macro.
                    // For now, this is a compile-time-resolved expression.
                    // Actually — we CAN'T know the nested column count at derive time.
                    // The nested struct's derive runs separately.
                    // We need to use the CPS macro approach or a different strategy.
                    //
                    // ALTERNATIVE: Use the EXPECTED_COLUMNS.len() const at the call site.
                    // Generate: $start + #current_offset + NestedType::EXPECTED_COLUMNS.len()
                    let inner_ty_ident = format_ident!("{}", inner_ty);
                    offset2 += 0; // placeholder — handled by marker below
                    expr
                }
                FieldKind::OptionalNested { inner_ty } => {
                    let nested_macro = format_ident!("__from_row_{}", inner_ty);
                    let expr = quote! {
                        #fname: if $row.is_null($start + #current_offset) {
                            None
                        } else {
                            Some(#nested_macro!($row, $start + #current_offset))
                        }
                    };
                    offset2 += 0; // placeholder
                    expr
                }
            }
        })
        .collect();

    // The correct approach: since we can't know nested column counts at derive time,
    // we generate the macro to use EXPECTED_COLUMNS.len() for each nested type.
    // Actually the simplest correct approach: track offsets using the struct's own
    // EXPECTED_COLUMNS, which is a flat list of ALL columns including nested.
    // The from_row implementation already knows the total flattened field order.

    // SIMPLEST APPROACH: Generate indexed access where index = position in EXPECTED_COLUMNS.
    // Since EXPECTED_COLUMNS is generated in the exact same field order, index i corresponds
    // to EXPECTED_COLUMNS[i]. We just need to count plain fields + nested fields' columns.

    // Let's use a different strategy: pre-compute all column indices at derive time
    // by counting recursively. But we can't — we don't have the nested struct's fields.

    // FINAL APPROACH: Keep the macro-based delegation but pass absolute indices.
    // Each struct's __from_row_ macro accepts ($row, $start) and offsets from $start.
    // Plain fields: $start + local_offset (local_offset known at derive time)
    // Nested fields: delegate to nested macro with $start + local_offset
    // The nested macro handles its own internal offsets from its own $start.
    // After the nested macro, we need to advance by the nested struct's column count.
    // We use NestedType::EXPECTED_COLUMNS.len() as a const expression in the macro.

    // This requires that the generated macro output uses const expressions, which
    // macro_rules! doesn't support for arithmetic. BUT: we can use a const block.

    // Actually the simplest: just pass a literal index for each field.
    // We know at derive time: plain fields = 1 column each.
    // For nested: we CAN'T know the count. We'd need a two-pass approach.

    // PRAGMATIC SOLUTION: Use the existing EXPECTED_COLUMNS ordering.
    // The FromRow impl knows the total column count. Generate a simple
    // sequential index in the impl, not in a macro_rules! macro.
    // Change FromRow::from_row to use direct indexed access.

    // Let me restructure: instead of generating __from_row_ as a macro_rules!,
    // generate it as part of the FromRow impl directly. This avoids the CPS
    // complexity and lets us use const arithmetic.

    // Actually — the __from_row_ macros exist specifically for nesting (CPS).
    // Without them, we can't compose nested structs.
    //
    // The key insight: when the parent struct derives FromRow, it generates its
    // own from_row() impl that calls row.get(0), row.get(1), etc. For nested
    // structs, it generates inline field construction:
    //   NestedType { field1: row.get(N), field2: row.get(N+1), ... }
    // But it doesn't know N without knowing the parent's field count up to that point.
    //
    // WITH MACROS: The macro delegates to the nested type's macro which handles
    // its own fields starting at the passed offset.
    //
    // The macro_rules! approach DOES work if we accept that $start + #offset
    // is evaluated at compile time (which it is — usize arithmetic on constants).

    // Let me try the simplest version and see if Rust accepts it.
    // macro_rules! with: $row.get($start + 0usize), $row.get($start + 1usize), etc.
    // For nested: nested_macro!($row, $start + Nusize) where N = count of plain fields before it.
    // After nested: next field is at $start + N + NestedType::EXPECTED_COLUMNS.len()
    // This DOESN'T work in macro_rules! because we can't do arithmetic in expr position
    // with a mix of $start (a macro variable) and a const.
    //
    // $start + 5 works (5 is a literal). But $start + Unit::EXPECTED_COLUMNS.len()
    // won't work as a macro pattern — it would need to be computed at expansion time.
    //
    // ACTUAL SIMPLEST SOLUTION: Don't use __from_row_ macros for indexed access.
    // Instead, generate the from_row() body directly in the FromRow impl,
    // using the known EXPECTED_COLUMNS ordering to assign indices.
    // For nested structs, inline the field construction.
    //
    // But this breaks the CPS pattern that handles arbitrary nesting depth...
    //
    // For our codebase (max 1 level of nesting — Product has Unit), this is fine.
    // Generate from_row() with hardcoded indices based on field order.

    // Let me just keep the original string-based approach for the macro delegation,
    // but change the FromRow impl to use indexed access for the top-level call.
    // The __from_row_ macro is only used for nested structs, which are rare.

    // KEEP EXISTING __from_row_ MACRO (string-based, for CPS nesting)
    // CHANGE FromRow::from_row() impl to use row.get(index) for its own fields

    // This is the pragmatic approach. Let me revert to the original macro:

    let field_exprs_str: Vec<_> = fields
        .iter()
        .map(|f| {
            let fname = &f.name;
            let fname_str = fname.to_string();
            match &f.kind {
                FieldKind::Plain { .. } => {
                    quote! { #fname: $row.get(concat!($prefix, #fname_str)) }
                }
                FieldKind::Nested { inner_ty } => {
                    let nested_macro = format_ident!("__from_row_{}", inner_ty);
                    let prefix_str = format!("{}_", fname_str);
                    quote! { #fname: #nested_macro!($row, concat!($prefix, #prefix_str)) }
                }
                FieldKind::OptionalNested { inner_ty } => {
                    let nested_macro = format_ident!("__from_row_{}", inner_ty);
                    let prefix_str = format!("{}_", fname_str);
                    let sentinel_col = format!("{}_id", fname_str);
                    quote! {
                        #fname: if $row.is_null(concat!($prefix, #sentinel_col)) {
                            None
                        } else {
                            Some(#nested_macro!($row, concat!($prefix, #prefix_str)))
                        }
                    }
                }
            }
        })
        .collect();

    quote! {
        #[macro_export]
        macro_rules! #macro_name {
            ($row:expr, $prefix:expr) => {
                #struct_name {
                    #(#field_exprs_str),*
                }
            };
        }
    }
}

// ─── __columns_StructName! (CPS) ────────────────────────────────────────

fn gen_columns_macro(
    struct_name: &syn::Ident,
    plain: &[&ParsedField],
    nested: &[&ParsedField],
) -> proc_macro2::TokenStream {
    let macro_name = format_ident!("__columns_{}", struct_name);
    let plain_names: Vec<String> = plain.iter().map(|f| f.name.to_string()).collect();
    let plain_concats: Vec<proc_macro2::TokenStream> = plain_names
        .iter()
        .map(|n| quote! { concat!($prefix, #n) })
        .collect();

    let mut out = proc_macro2::TokenStream::new();

    if nested.is_empty() {
        // Leaf struct: add plain fields, call callback directly.
        out.extend(quote! {
            #[macro_export]
            macro_rules! #macro_name {
                ([$($acc:expr),*], $prefix:expr, $callback:ident $(, $($rest:tt)*)?) => {
                    $callback!([
                        $($acc,)*
                        #(#plain_concats),*
                    ] $(, $($rest)*)?)
                };
            }
        });
    } else {
        // Has nested fields: delegate to first nested, with continuation chain.
        let first_ty = nested_type_ident(nested[0]);
        let first_prefix = format!("{}_", nested[0].name);
        let first_macro = format_ident!("__columns_{}", first_ty);

        let (cb, pt) = if nested.len() == 1 {
            (quote! { $callback }, quote! { $(, $($rest)*)? })
        } else {
            let cont = format_ident!("__columns_{}__cont_0", struct_name);
            (
                quote! { #cont },
                quote! { , $prefix, $callback $(, $($rest)*)? },
            )
        };

        out.extend(quote! {
            #[macro_export]
            macro_rules! #macro_name {
                ([$($acc:expr),*], $prefix:expr, $callback:ident $(, $($rest:tt)*)?) => {
                    #first_macro!(
                        [$($acc,)* #(#plain_concats),*],
                        concat!($prefix, #first_prefix),
                        #cb
                        #pt
                    )
                };
            }
        });

        // Continuation macros for subsequent nested fields.
        for i in 0..nested.len().saturating_sub(1) {
            let cont_name = format_ident!("__columns_{}__cont_{}", struct_name, i);
            let next = nested[i + 1];
            let next_ty = nested_type_ident(next);
            let next_prefix = format!("{}_", next.name);
            let next_macro = format_ident!("__columns_{}", next_ty);

            let is_last = i + 1 == nested.len() - 1;
            let (cb, pt) = if is_last {
                (quote! { $callback }, quote! { $(, $($rest)*)? })
            } else {
                let next_cont = format_ident!("__columns_{}__cont_{}", struct_name, i + 1);
                (
                    quote! { #next_cont },
                    quote! { , $prefix, $callback $(, $($rest)*)? },
                )
            };

            out.extend(quote! {
                #[macro_export]
                macro_rules! #cont_name {
                    ([$($acc:expr),*], $prefix:expr, $callback:ident $(, $($rest:tt)*)?) => {
                        #next_macro!(
                            [$($acc),*],
                            concat!($prefix, #next_prefix),
                            #cb
                            #pt
                        )
                    };
                }
            });
        }
    }

    out
}

// ─── __column_types_StructName! (CPS) ───────────────────────────────────

fn gen_column_types_macro(
    struct_name: &syn::Ident,
    plain: &[&ParsedField],
    nested: &[&ParsedField],
) -> proc_macro2::TokenStream {
    let macro_name = format_ident!("__column_types_{}", struct_name);

    let plain_type_exprs: Vec<proc_macro2::TokenStream> = plain
        .iter()
        .map(|f| {
            let col = f.name.to_string();
            let ty = match &f.kind {
                FieldKind::Plain { rust_type } => rust_type.as_str(),
                _ => unreachable!(),
            };
            // For Option<T> fields, nullable is always true regardless of $optional.
            // For non-Option fields inside a LEFT JOIN ($optional=true), they become nullable.
            let nullable = f.is_optional;
            if nullable {
                quote! { (concat!($prefix, #col), #ty, true) }
            } else {
                quote! { (concat!($prefix, #col), #ty, $optional) }
            }
        })
        .collect();

    let mut out = proc_macro2::TokenStream::new();

    if nested.is_empty() {
        out.extend(quote! {
            #[macro_export]
            macro_rules! #macro_name {
                ([$($acc:expr),*], $prefix:expr, $optional:expr, $callback:ident $(, $($rest:tt)*)?) => {
                    $callback!([
                        $($acc,)*
                        #(#plain_type_exprs),*
                    ] $(, $($rest)*)?)
                };
            }
        });
    } else {
        let first = nested[0];
        let first_ty = nested_type_ident(first);
        let first_prefix = format!("{}_", first.name);
        let first_macro = format_ident!("__column_types_{}", first_ty);
        let first_optional = if is_optional_nested(first) {
            quote! { true }
        } else {
            quote! { $optional }
        };

        let (cb, pt) = if nested.len() == 1 {
            (quote! { $callback }, quote! { $(, $($rest)*)? })
        } else {
            let cont = format_ident!("__column_types_{}__cont_0", struct_name);
            (
                quote! { #cont },
                quote! { , $prefix, $optional, $callback $(, $($rest)*)? },
            )
        };

        out.extend(quote! {
            #[macro_export]
            macro_rules! #macro_name {
                ([$($acc:expr),*], $prefix:expr, $optional:expr, $callback:ident $(, $($rest:tt)*)?) => {
                    #first_macro!(
                        [$($acc,)* #(#plain_type_exprs),*],
                        concat!($prefix, #first_prefix),
                        #first_optional,
                        #cb
                        #pt
                    )
                };
            }
        });

        for i in 0..nested.len().saturating_sub(1) {
            let cont_name = format_ident!("__column_types_{}__cont_{}", struct_name, i);
            let next = nested[i + 1];
            let next_ty = nested_type_ident(next);
            let next_prefix = format!("{}_", next.name);
            let next_macro = format_ident!("__column_types_{}", next_ty);
            let next_optional = if is_optional_nested(next) {
                quote! { true }
            } else {
                quote! { $optional }
            };

            let is_last = i + 1 == nested.len() - 1;
            let (cb, pt) = if is_last {
                (quote! { $callback }, quote! { $(, $($rest)*)? })
            } else {
                let next_cont = format_ident!("__column_types_{}__cont_{}", struct_name, i + 1);
                (
                    quote! { #next_cont },
                    quote! { , $prefix, $optional, $callback $(, $($rest)*)? },
                )
            };

            out.extend(quote! {
                #[macro_export]
                macro_rules! #cont_name {
                    ([$($acc:expr),*], $prefix:expr, $optional:expr, $callback:ident $(, $($rest:tt)*)?) => {
                        #next_macro!(
                            [$($acc),*],
                            concat!($prefix, #next_prefix),
                            #next_optional,
                            #cb
                            #pt
                        )
                    };
                }
            });
        }
    }

    out
}

// ─── impl block ─────────────────────────────────────────────────────────

fn gen_impl_block(struct_name: &syn::Ident, fields: &[ParsedField]) -> proc_macro2::TokenStream {
    let columns_macro = format_ident!("__columns_{}", struct_name);
    let types_macro = format_ident!("__column_types_{}", struct_name);
    let from_row_macro = format_ident!("__from_row_{}", struct_name);

    let columns_finalize = format_ident!("__columns_finalize_{}", struct_name);
    let types_finalize = format_ident!("__column_types_finalize_{}", struct_name);

    // Check if this struct has any nested fields.
    let has_nested = fields.iter().any(|f| !matches!(f.kind, FieldKind::Plain { .. }));

    // For flat structs (no nesting): generate indexed access for faster deserialization.
    // For nested structs: use string-based macro delegation (CPS pattern needed).
    let from_row_body = if has_nested {
        // Nested: use the string-based __from_row_ macro
        quote! { #from_row_macro!(row, "") }
    } else {
        // Flat: generate direct indexed access — row.get(0), row.get(1), etc.
        let field_getters: Vec<_> = fields.iter().enumerate().map(|(i, f)| {
            let fname = &f.name;
            quote! { #fname: row.get(#i) }
        }).collect();
        quote! { #struct_name { #(#field_getters),* } }
    };

    quote! {
        #[macro_export]
        macro_rules! #columns_finalize {
            ([$($item:expr),*]) => { &[$($item),*] };
        }

        #[macro_export]
        macro_rules! #types_finalize {
            ([$($item:expr),*]) => { &[$($item),*] };
        }

        impl #struct_name {
            pub const EXPECTED_COLUMNS: &[&str] =
                #columns_macro!([], "", #columns_finalize);
            pub const COLUMN_TYPES: &[(&str, &str, bool)] =
                #types_macro!([], "", false, #types_finalize);
        }

        impl from_row::FromRow for #struct_name {
            fn from_row(row: &tokio_postgres::Row) -> Self {
                #from_row_body
            }
        }
    }
}
