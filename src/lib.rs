// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Useful procedural macros
//!
//! Two independent features, one module each. Four attributes, in pairs:
//!
//! - `stack_safe` — [`#[stack_safe]`](macro@stack_safe) rewrites a recursive function
//!   into an iterative state machine whose frames live on the heap, so recursion depth is
//!   bounded by available memory rather than by the native stack. Functions that recurse
//!   *through each other* need scanning together, so the attribute also goes on a module
//!   or an impl block, works out which of them form cycles, and gives each cycle one
//!   shared driver. Either way it scans the whole scope, down to the functions declared
//!   inside a body.
//! - `delegate_trait` — [`#[delegatable_trait]`](macro@delegatable_trait) on the trait
//!   and [`#[delegate_trait]`](macro@delegate_trait) on the impl forward the trait
//!   methods you did *not* write to an inner field.
//!
//! `README.md` covers the motivation, the design, and — for `stack_safe` — exactly
//! what it does and does not preserve.

extern crate proc_macro;

use proc_macro2::TokenStream;

mod delegate_trait;
mod stack_safe;

/// Rewrite a recursive function so it does not consume native stack.
///
/// ```
/// use yaspar_macros::stack_safe;
///
/// enum Node {
///     Leaf(u64),
///     Branch(Vec<usize>),
/// }
///
/// #[stack_safe]
/// fn sum(nodes: &[Node], i: usize) -> u64 {
///     match &nodes[i] {
///         Node::Leaf(v) => *v,
///         Node::Branch(kids) => {
///             let mut acc = 0;
///             for &k in kids {
///                 acc += sum(nodes, k);
///             }
///             acc
///         }
///     }
/// }
///
/// // An arena, so that building and dropping the tree is iterative too.
/// let nodes = vec![Node::Branch(vec![1, 2]), Node::Leaf(3), Node::Leaf(4)];
/// assert_eq!(sum(&nodes, 0), 7);
/// ```
///
/// Every recursive call is split into a *request* to evaluate the body on new
/// arguments and a *continuation* holding the rest of the body; a driver loop keeps
/// the pending continuations in a `Vec`. `README.md` documents the supported subset
/// of Rust.
///
/// `&mut` parameters and `&self` / `&mut self` methods work: those travel through
/// the driver as a context it owns and lends out, rather than in the argument
/// payload, so they stay usable after a recursive call returns.
///
/// # Options
///
/// `#[stack_safe(use_nonlinear_mut)]` additionally allows a recursive call to
/// pass a reference *derived* from a `&mut` parameter, as in
/// `walk(&mut t.kids[i])`. The slot then holds a raw pointer that is parked for the
/// child's subtree and restored by its continuation. The macro checks what it can
/// see — the argument must be `&mut <place>` rooted at a context parameter — but it
/// has no types, so soundness rests on that place outliving the call; see
/// `README.md`.
///
/// `#[stack_safe(data_in_frame)]` allows a recursive call to lend the callee a value
/// built at the call site, as in `rec(n, &Node::Cons(v, rest))`. Natively that
/// temporary survives because the caller's frame does; here the call becomes a
/// `return`, so the value is moved into a store the driver owns and the callee reaches
/// it through a raw pointer. The store keeps it at a fixed address until the frame that
/// built it is popped. In exchange, the callee must not return or stash anything
/// borrowing that value, which borrowck still refuses; see `README.md`.
///
/// Both options hand the driver a raw pointer where the original had a reference, which does not put
/// the borrow checker aside: whatever the original asked of it is still checked, so a program it
/// would have refused is still refused.
///
/// Both options can apply to one call, and to one *argument list*: a child may be handed a
/// place derived from a `&mut` parameter and, beside it, a reference to a value built there.
/// The slot is parked for the child's subtree and the value is moved into the store, and the
/// continuation undoes both — the pointer first, so that what the body re-derives from the slot
/// is the parent's again.
///
/// # Functions declared in the body
///
/// A body is a scope of item definitions like any other, so the scan covers it: a `fn`
/// nested in the body is rewritten too, and a cycle running through one — `depth` calling
/// `step` and `step` calling `depth` — is flattened like any other cycle, to any depth of
/// nesting. A nested function that does not recurse is left exactly as written.
///
/// ```
/// use yaspar_macros::stack_safe;
///
/// #[stack_safe]
/// fn depth(n: u64) -> u64 {
///     fn step(n: u64) -> u64 {
///         if n == 0 { 0 } else { 1 + depth(n - 1) }
///     }
///     if n == 0 { 0 } else { 1 + step(n - 1) }
/// }
///
/// assert_eq!(depth(1_000_000), 1_000_000);
/// ```
///
/// A cycle's driver is written where its outermost member was declared. A member from
/// further in goes *inside* that driver, under the name it had: a name scoped to a body is
/// nobody else's, so nothing is exposed outside the body that declared it, and whatever
/// called it there still finds it.
///
/// Such a cycle cannot be generic, though, nor name a lifetime of its own: the driver carries those
/// parameters and a function declared in a body can never name them, seeing none of the generics of
/// the one hosting it. Nor can a member take an `impl Trait` parameter, or a `Self` the driver's
/// signature cannot spell. A trait object is no obstacle — `&dyn Trait` is a type the driver can
/// name. Moving the nested function out to the enclosing scope lifts the restriction, since it is
/// then a member like any other.
///
/// ```compile_fail
/// # use yaspar_macros::stack_safe;
/// // error: `step` is declared inside the body of a function it recurses with, so the driver
/// // they share has to be written outside that body — and the driver they share is generic,
/// // which a function declared in a body cannot be: it cannot name the parameters of the one
/// // hosting it ...
/// #[stack_safe]
/// fn depth<T: Copy>(n: u64, t: T) -> u64 {
///     fn step(n: u64) -> u64 { if n == 0 { 0 } else { depth(n - 1, 0u8) } }
///     if n == 0 { 0 } else { 1 + step(n - 1) }
/// }
/// ```
///
/// # On a module or an impl block
///
/// Placed on either, every function inside that recurses — alone
/// or through the others — is rewritten, and the rest pass through untouched.
///
/// A per-function attribute cannot flatten mutual recursion, because expanding `f`
/// needs `g`'s body to turn `g(..)` into an entry into the same driver. On the
/// container the macro sees every body, so it builds the call graph, takes its
/// transitive closure, and gives each cycle one shared driver.
///
/// Nested modules and impl blocks are descended into to any depth, each grouped on
/// its own, so one attribute covers a whole module tree. Methods join a cycle through
/// `self.g(..)` or `Self::g(self, ..)`.
///
/// A trait impl is covered too, so long as none of its own members recurses. A rewritten member
/// needs a plain associated function beside it to carry the body, and a trait impl may hold nothing
/// but the trait's own members, so a recursive one is rejected by name. A recursion declared inside
/// a member's body is fine, since its driver is written in that body.
///
/// The annotated module also threads its top-level functions back out, by re-exporting
/// each of them beside itself, so `f(..)` works at the attribute's own scope and not only
/// `m::f(..)`. A `use` rather than a forwarding definition: it never has to reproduce a
/// signature, so a generic, a where-clause, or a type only the module can name all come
/// along for free. Visibility is re-expressed rather than copied, so no name out-reaches
/// its module. Private functions are skipped, and nested modules are not lifted.
///
/// ```
/// use yaspar_macros::stack_safe;
///
/// #[stack_safe]
/// mod parity {
///     pub fn is_even(n: u64) -> bool {
///         if n == 0 { true } else { is_odd(n - 1) }
///     }
///     pub fn is_odd(n: u64) -> bool {
///         if n == 0 { false } else { is_even(n - 1) }
///     }
///     // Not recursive: emitted as written.
///     pub fn describe(n: u64) -> &'static str {
///         if is_even(n) { "even" } else { "odd" }
///     }
/// }
///
/// // Both the module path and the threaded-out name work.
/// assert!(parity::is_even(1_000_000));
/// assert!(is_even(1_000_000));
/// assert_eq!(describe(7), "odd");
/// ```
///
/// Members of one cycle share a driver, so they must agree on their `&mut` parameters,
/// and a mismatch is reported by name.
///
/// They need *not* agree on their return type. The driver has one result, so the members
/// answer with a union of their return types and each keeps its own signature:
///
/// ```
/// use yaspar_macros::stack_safe;
///
/// #[stack_safe]
/// mod m {
///     pub fn is_even(n: u64) -> bool {
///         if n == 0 { true } else { count(n - 1) % 2 == 1 }
///     }
///     pub fn count(n: u64) -> u64 {
///         if n == 0 { 0 } else if is_even(n - 1) { 1 } else { 2 }
///     }
/// }
///
/// assert!(m::is_even(4));
/// assert_eq!(m::count(4), 2);
/// ```
/// A group's members answer through one driver, so their return types have to be
/// nameable. An `impl Trait` return is its own opaque type, so a group of more than one
/// member cannot have one — not even if every member spells the same trait.
///
/// ```compile_fail
/// # use yaspar_macros::stack_safe;
/// #[stack_safe]
/// mod m {
///     // error: `up` is part of a mutually recursive group ... an `impl Trait` return is
///     // its own opaque type and cannot be named
///     pub fn up(n: u64) -> impl Iterator<Item = u64> {
///         if n == 0 { 0..1 } else { let k = down(n - 1); 0..k }
///     }
///     pub fn down(n: u64) -> u64 { if n == 0 { 1 } else { up(n - 1).count() as u64 } }
/// }
/// ```
///
/// The members of a lifted group share one seed, whose fields are their parameters, so a
/// parameter type has to be nameable beside them. An alias that *hides* a reference is
/// the one shape the macro cannot tell from an ordinary type, and the seed then has no
/// lifetime to give that field. Writing the elision out — `w: Words<'_>` — fixes it.
///
/// ```compile_fail
/// # use yaspar_macros::stack_safe;
/// type Words<'a> = &'a [&'a str];
/// #[stack_safe]
/// mod m {
///     use super::Words;
///     // error[E0106]: missing lifetime specifier
///     pub fn count(w: Words, i: usize) -> usize {
///         if i >= w.len() { 0 } else { 1 + other(w, i + 1) }
///     }
///     pub fn other(w: Words, i: usize) -> usize { count(w, i) }
/// }
/// ```
///
/// Options are accepted here too — `#[stack_safe(use_nonlinear_mut)]` — and on individual
/// functions inside, where they are scoped like bindings: a marker says what is in force for the
/// function it is written on and whatever that function's body declares, shadowing the enclosing
/// options rather than adding to them, and reaching nothing outside.
/// A recursive call the macro cannot see through is rejected rather than left to
/// recurse natively — inside a closure, or inside a macro invocation in either
/// statement or expression position. A closure that does *not* recurse is ordinary
/// code and passes through untouched:
///
/// ```compile_fail
/// # use yaspar_macros::stack_safe;
/// // error: cannot rewrite a recursive call inside a closure
/// #[stack_safe]
/// fn f(n: u64) -> u64 { (0..n).map(|k| f(k)).sum() }
/// ```
///
///
/// ```compile_fail
/// # use yaspar_macros::stack_safe;
/// // error: possible recursive call to `f` inside a macro invocation
/// #[stack_safe]
/// fn f(n: u64) -> u64 {
///     if n == 0 { 0 } else { println!("{}", f(n - 1)); 0 }
/// }
/// ```
///
/// ```compile_fail
/// # use yaspar_macros::stack_safe;
/// // error: `f` has no effect: nothing in its scope recurses
/// #[stack_safe]
/// fn f(n: u64) -> u64 { if n == 0 { 0 } else { g(n - 1) } }
/// fn g(n: u64) -> u64 { n }
/// ```
///
/// ```compile_fail
/// # use yaspar_macros::stack_safe;
/// enum Chain<'a> { Nil, Cons(u64, &'a Chain<'a>) }
/// // error: cannot pass a reference to a value built here ... opt in with
/// // `#[stack_safe(data_in_frame)]`
/// #[stack_safe]
/// fn grow(n: usize, c: &Chain<'_>) -> usize {
///     if n == 0 { 0 } else { grow(n - 1, &Chain::Cons(1, c)) }
/// }
/// ```
///
/// ```compile_fail
/// # use yaspar_macros::stack_safe;
/// struct Tree { kids: Vec<Tree> }
/// // error: cannot pass a reference derived from `t` ... opt in with
/// // `#[stack_safe(use_nonlinear_mut)]`
/// #[stack_safe]
/// fn bump(t: &mut Tree) {
///     for i in 0..t.kids.len() { bump(&mut t.kids[i]); }
/// }
/// ```
#[proc_macro_attribute]
pub fn stack_safe(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    finish(stack_safe::expand_attr(attr.into(), item.into()))
}

/// Placed on a trait definition, to make the trait usable with
/// `#[delegate_trait]`.
///
/// The trait is emitted unchanged, alongside a hidden helper macro that records
/// its required method signatures.
///
/// # Options
///
/// `#[delegatable_trait(local)]` keeps that helper in the trait's own module instead
/// of `#[macro_export]`ing it to the crate root. Use it when two traits of the same
/// name in one crate must both be delegatable, since the default form makes the
/// second one collide with `E0428`. Nothing changes on the impl side.
///
/// The trade is that a non-exported macro cannot leave its crate — `pub use` of one is
/// `E0364` — so a `local` trait cannot be delegated from another crate at all; the
/// attempt is an `E0603` naming the private macro. Delegating across crates therefore
/// needs the default form.
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

/// Placed on `impl Trait for Struct { .. }`: the methods written in the block are
/// kept, every other required method of `Trait` is delegated to `self.<target>`.
///
/// The trait must carry `#[delegatable_trait]`.
/// `target` is a *field name*, not an expression: `target = inner`, not
/// `target = self.inner`.
///
/// The helper macro is reached through the trait's own path, so a trait from another
/// crate is delegated with nothing to import:
///
/// ```ignore
/// #[delegate_trait(target = inner)]
/// impl other_crate::a::Store for Wrapper {}
/// ```
///
/// Naming the trait *bare*, after importing it, leaves no path to follow, and that form
/// only works within the crate that defines the trait. Write the path instead when the
/// trait comes from a dependency.
///
/// Methods with a default body in the trait are not delegated — they keep their
/// default implementation unless the impl block overrides them.
///
/// A generic trait works, with parameters of any kind — lifetimes, types and
/// consts. Their names have no binding at the impl site, so the helper macro records
/// them as metavariables and this attribute passes the impl's trait arguments
/// positionally. A defaulted parameter may be left out, since the trait knows what to
/// fill in.
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
/// #[delegate_trait(target = inner)]
/// impl Keyed<u32> for Wrapper {}
///
/// let mut w = Wrapper { inner: Inner(1) };
/// w.put(0, 5);
/// assert_eq!(w.get(2), 7);
/// ```
///
/// ```
/// use yaspar_macros::{delegatable_trait, delegate_trait};
///
/// #[delegatable_trait]
/// trait Math {
///     fn add(&self, a: u32, b: u32) -> u32;
///     fn mul(&self, a: u32, b: u32) -> u32;
/// }
///
/// struct MathImpl;
/// impl Math for MathImpl {
///     fn add(&self, a: u32, b: u32) -> u32 { a + b }
///     fn mul(&self, a: u32, b: u32) -> u32 { a * b }
/// }
///
/// struct MathWrapper { inner: MathImpl }
///
/// // An empty block delegates everything.
/// #[delegate_trait(target = inner)]
/// impl Math for MathWrapper {}
///
/// let w = MathWrapper { inner: MathImpl };
/// assert_eq!(w.add(2, 3), 5);
/// assert_eq!(w.mul(4, 5), 20);
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
