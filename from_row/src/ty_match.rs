// Compile-time type matching for bind parameter assertions.
//
// Ported from sqlx (MIT license): https://github.com/launchbadge/sqlx/blob/main/src/ty_match.rs
//
// These types allow `query_as!()` and `query_scalar!()` to compare a given
// parameter's type to an expected type even if the former is behind a reference
// or wrapped in `Option`.
//
// Uses autoref-based specialization: for method calls, the compiler adds
// reference ops until it finds a matching impl. With impls that technically
// don't overlap, this acts as a hacky form of specialization (works only when
// all types are statically known, i.e. not in a generic context).

use std::marker::PhantomData;

#[allow(clippy::just_underscores_and_digits)]
pub fn same_type<T>(_1: &T, _2: &T) {}

// ── WrapSame ────────────────────────────────────────────────────────────────
//
// If the bind expression is `Option<U>`, `wrap_same()` returns `Option<T>`.
// If the bind expression is `U` (non-Option), `wrap_same()` returns `T`.

pub struct WrapSame<T, U>(PhantomData<T>, PhantomData<U>);

impl<T, U> WrapSame<T, U> {
    pub fn new(_arg: &U) -> Self {
        WrapSame(PhantomData, PhantomData)
    }
}

pub trait WrapSameExt: Sized {
    type Wrapped;

    fn wrap_same(self) -> Self::Wrapped {
        panic!("only for type resolution")
    }
}

// Option case: WrapSame<T, Option<U>> → Option<T>
impl<T, U> WrapSameExt for WrapSame<T, Option<U>> {
    type Wrapped = Option<T>;
}

// Non-Option case (via autoref): &WrapSame<T, U> → T
impl<T, U> WrapSameExt for &'_ WrapSame<T, U> {
    type Wrapped = T;
}

// ── MatchBorrow ─────────────────────────────────────────────────────────────
//
// Handles coercions like &str ↔ String, &[u8] ↔ Vec<u8>, and reference depth.

pub struct MatchBorrow<T, U>(PhantomData<T>, PhantomData<U>);

impl<T, U> MatchBorrow<T, U> {
    pub fn new(t: T, _u: &U) -> (T, Self) {
        (t, MatchBorrow(PhantomData, PhantomData))
    }
}

pub trait MatchBorrowExt: Sized {
    type Matched;

    fn match_borrow(self) -> Self::Matched {
        panic!("only for type resolution")
    }
}

// &str ↔ String coercions
impl<'a> MatchBorrowExt for MatchBorrow<Option<&'a str>, Option<String>> {
    type Matched = Option<&'a str>;
}

impl<'a> MatchBorrowExt for MatchBorrow<Option<&'a [u8]>, Option<Vec<u8>>> {
    type Matched = Option<&'a [u8]>;
}

impl<'a> MatchBorrowExt for MatchBorrow<Option<&'a str>, Option<&'a String>> {
    type Matched = Option<&'a str>;
}

impl<'a> MatchBorrowExt for MatchBorrow<Option<&'a [u8]>, Option<&'a Vec<u8>>> {
    type Matched = Option<&'a [u8]>;
}

impl<'a> MatchBorrowExt for MatchBorrow<&'a str, String> {
    type Matched = &'a str;
}

impl<'a> MatchBorrowExt for MatchBorrow<&'a [u8], Vec<u8>> {
    type Matched = &'a [u8];
}

// Reference depth coercions
impl<T> MatchBorrowExt for MatchBorrow<&'_ T, T> {
    type Matched = T;
}

impl<T> MatchBorrowExt for MatchBorrow<&'_ &'_ T, T> {
    type Matched = T;
}

impl<T> MatchBorrowExt for MatchBorrow<T, &'_ T> {
    type Matched = T;
}

impl<T> MatchBorrowExt for MatchBorrow<T, &'_ &'_ T> {
    type Matched = T;
}

impl<T> MatchBorrowExt for MatchBorrow<Option<&'_ T>, Option<T>> {
    type Matched = Option<T>;
}

impl<T> MatchBorrowExt for MatchBorrow<Option<&'_ &'_ T>, Option<T>> {
    type Matched = Option<T>;
}

impl<T> MatchBorrowExt for MatchBorrow<Option<T>, Option<&'_ T>> {
    type Matched = Option<T>;
}

impl<T> MatchBorrowExt for MatchBorrow<Option<T>, Option<&'_ &'_ T>> {
    type Matched = Option<T>;
}

// Fallback (via autoref): identity
impl<T, U> MatchBorrowExt for &'_ MatchBorrow<T, U> {
    type Matched = U;
}

// ── Helpers ─────────────────────────────────────────────────────────────────

pub fn conjure_value<T>() -> T {
    panic!("only for type resolution")
}

pub fn dupe_value<T>(_t: &T) -> T {
    panic!("only for type resolution")
}
