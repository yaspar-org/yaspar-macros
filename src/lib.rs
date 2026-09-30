// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Useful procedural macros.
//!
//! - [`#[stack_safe]`](macro@stack_safe): run recursive functions without growing the
//!   native stack.
//! - [`#[delegatable_trait]`](macro@delegatable_trait) and
//!   [`#[delegate_trait]`](macro@delegate_trait): forward unimplemented trait methods to
//!   an inner field.
//!
//! See `README.md` for details.

extern crate proc_macro;

use proc_macro2::TokenStream;

mod delegate_trait;
mod stack_safe;

/// Rewrite recursive functions to keep their frames on the heap instead of the native
/// stack, so recursion depth is bounded by memory, not stack size.
///
/// ```
/// use yaspar_macros::stack_safe;
///
/// #[stack_safe]
/// fn depth(n: u64) -> u64 {
///     if n == 0 { 0 } else { 1 + depth(n - 1) }
/// }
///
/// assert_eq!(depth(1_000_000), 1_000_000);
/// ```
///
/// `&mut` parameters and `&self` / `&mut self` methods are supported. Functions nested in
/// a body are scanned and rewritten too. See `README.md` for the supported subset of Rust.
///
/// # Mutual recursion
///
/// Put the attribute on a `mod` or `impl` block. Every function in it that recurses, alone
/// or through others, is rewritten; the rest are left as written. Each non-private top-level
/// function of an annotated module is also re-exported beside the module.
///
/// ```
/// use yaspar_macros::stack_safe;
///
/// #[stack_safe]
/// mod parity {
///     pub fn is_even(n: u64) -> bool { if n == 0 { true } else { is_odd(n - 1) } }
///     pub fn is_odd(n: u64) -> bool { if n == 0 { false } else { is_even(n - 1) } }
/// }
///
/// assert!(parity::is_even(1_000_000));
/// assert!(is_odd(7));
/// ```
///
/// Functions in one cycle may have different return types but must share the same `&mut`
/// parameters. A cycle of two or more functions cannot return `impl Trait`.
///
/// # Options
///
/// - `use_nonlinear_mut`: allow passing a reborrow of a `&mut` parameter, e.g.
///   `walk(&mut t.kids[i])`.
/// - `data_in_frame`: allow passing a reference to a value built at the call site, e.g.
///   `rec(&Node::Cons(v, rest))`. The callee must not return or keep that reference.
///
/// Options can go on a container or on individual functions inside it; the innermost wins.
///
/// # Rejected
///
/// - Recursive calls inside a closure or a macro invocation.
/// - An attribute on something that does not recurse.
/// - A cycle through a function declared inside a body that is generic, names its own
///   lifetime, or takes an `impl Trait` parameter (move the function out instead).
/// - Recursive members of a trait impl.
/// - In a cycle of two or more, a parameter whose type alias hides a lifetime: write
///   `Words<'_>`, not `Words`.
///
/// ```compile_fail
/// # use yaspar_macros::stack_safe;
/// // error: cannot rewrite a recursive call inside a closure
/// #[stack_safe]
/// fn f(n: u64) -> u64 { (0..n).map(|k| f(k)).sum() }
/// ```
#[proc_macro_attribute]
pub fn stack_safe(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    finish(stack_safe::expand_attr(attr.into(), item.into()))
}

/// Make a trait usable with [`#[delegate_trait]`](macro@delegate_trait).
///
/// The trait is emitted unchanged, plus a hidden helper macro recording its required methods.
///
/// # Options
///
/// `local`: keep the helper in the trait's module instead of exporting it from the crate root.
/// Use it when two delegatable traits share a name in one crate (otherwise `E0428`). A `local`
/// trait cannot be delegated from another crate.
///
/// ```
/// use yaspar_macros::{delegatable_trait, delegate_trait};
///
/// #[delegatable_trait]
/// trait Greet {
///     fn hello(&self) -> String;
///     fn goodbye(&self) -> String;
/// }
///
/// struct Inner;
/// impl Greet for Inner {
///     fn hello(&self) -> String { "hello from inner".into() }
///     fn goodbye(&self) -> String { "goodbye from inner".into() }
/// }
///
/// struct Wrapper { inner: Inner }
///
/// // Only `hello` is overridden; `goodbye` is delegated to `self.inner`.
/// #[delegate_trait(target = inner)]
/// impl Greet for Wrapper {
///     fn hello(&self) -> String { "hello from wrapper".into() }
/// }
///
/// let w = Wrapper { inner: Inner };
/// assert_eq!(w.hello(), "hello from wrapper");
/// assert_eq!(w.goodbye(), "goodbye from inner");
/// ```
#[proc_macro_attribute]
pub fn delegatable_trait(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    finish(delegate_trait::expand_trait_def(attr.into(), item.into()))
}

/// On `impl Trait for Struct`, forward every required method not written in the block to
/// `self.<target>`. The trait must carry
/// [`#[delegatable_trait]`](macro@delegatable_trait).
///
/// - `target` is a field path (`inner`, `0`, `inner.deep`), not an expression like `self.inner`.
/// - Methods with a default body keep it unless overridden.
/// - Generic traits (lifetime, type, const parameters) work; defaulted ones may be omitted.
/// - For a trait from another crate, name it by path (`impl other::Store for W {}`), not
///   through a `use`.
///
/// ```
/// use yaspar_macros::{delegatable_trait, delegate_trait};
///
/// #[delegatable_trait]
/// trait Keyed<K> {
///     fn get(&self, k: K) -> u64;
///     fn put(&mut self, k: K, v: u64);
/// }
///
/// struct Inner(u64);
/// impl Keyed<u32> for Inner {
///     fn get(&self, k: u32) -> u64 { self.0 + k as u64 }
///     fn put(&mut self, _k: u32, v: u64) { self.0 = v; }
/// }
///
/// struct Wrapper { inner: Inner }
///
/// // An empty block delegates everything.
/// #[delegate_trait(target = inner)]
/// impl Keyed<u32> for Wrapper {}
///
/// let mut w = Wrapper { inner: Inner(1) };
/// w.put(0, 5);
/// assert_eq!(w.get(2), 7);
/// ```
#[proc_macro_attribute]
pub fn delegate_trait(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    finish(delegate_trait::expand_trait_impl(attr.into(), item.into()))
}

fn finish(result: syn::Result<TokenStream>) -> proc_macro::TokenStream {
    match result {
        Ok(ts) => ts,
        Err(e) => e.to_compile_error(),
    }
    .into()
}
