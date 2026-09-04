// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! `#[stack_safe]` on a module or an impl block: work out which of the
//! functions inside recurse — alone or through each other — and give each cycle one
//! shared driver.
//!
//! # Why the attribute has to sit on the container
//!
//! An attribute on a single `fn` cannot flatten mutual recursion, because expanding
//! `f` needs `g`'s *body* to turn `g(..)` into an entry into the same driver, and a
//! per-function attribute never sees it. Naming the partner
//! (`#[stack_safe(with = g)]`) would not help for the same reason. The attribute
//! therefore goes on the enclosing item, where every body is in scope.
//!
//! # Scope of the scan
//!
//! Nested modules and impl blocks are descended into, to any depth, and each is
//! grouped on its own. Cycles are looked for *within* one container because a group's
//! arms end up in one place — the shared machine beside its members, or a copy of it
//! inside each — and an arm that names a private item of its own module would not
//! compile once written next to a member from elsewhere. A cycle that crosses
//! containers is therefore not grouped — it is the same silent footgun as any other
//! recursion the macro cannot see, and `README.md` says so.
//!
//! A member's *body* is descended into as well: `emit::scan_scope` takes the
//! container's functions and everything their bodies declare as one graph, so a cycle
//! is found wherever it runs. One declared entirely inside a body is rewritten there
//! and then; one that includes a member of the container comes back here, to be given
//! a driver beside the container's functions.
//!
//! # Finding the cycles
//!
//! One edge per syntactic call between the functions in scope — `g(..)` for a free
//! function, `self.g(..)` or `Self::g(self, ..)` for a method — with each name
//! resolved the way Rust resolves it, then the transitive closure: `f` and `g` belong
//! to the same group when each reaches the other, and `f` is recursive at all when it
//! reaches itself. Warshall's closure is O(n³) in the functions of one scope, which is
//! nothing at these sizes, and it is far harder to get wrong than a hand-rolled
//! Tarjan. See `scope.rs` for the graph itself.
//!
//! A function in no cycle is emitted untouched — unless its own body held one, which is
//! rewritten in place — so a container can hold a mix.
//!
//! # Threading the module's functions back out
//!
//! The annotated module holds the encoding — the drivers, the entry enums, the
//! rewritten bodies — so callers would otherwise have to name it on every call. Each
//! of its top-level functions is therefore re-exported at the attribute's own scope:
//!
//! ```text
//! #[stack_safe]              mod m {
//! mod m {                              pub fn f(n: u64) -> u64 { <driver> }
//!     pub fn f(n: u64) { .. g(..) }    pub fn g(n: u64) -> u64 { <driver> }
//!     pub fn g(n: u64) { .. f(..) }    fn helper(..) { .. }
//!     fn helper(..) { .. }         }
//! }                                pub use m::f;
//!                                  pub use m::g;
//! ```
//!
//! So `f(..)` works at the attribute's own scope, not just `m::f(..)`.
//!
//! A `use` rather than a forwarding definition, which would have to reproduce the
//! signature: that means spelling out generics, where-clauses and every parameter type,
//! and a type the module merely imports privately cannot be spelled outside it at all.
//! A re-export names no type, so all of that comes along for free.
//!
//! A function the module keeps private is skipped: it has no name to carry out, and a
//! `use` of a private item would not compile either.
//!
//! Only the annotated module's own functions are threaded out. A nested module is
//! grouped in place without lifting, since lifting it one level would still not put it
//! in reach of the outer caller.
//!
//! # How a group is emitted
//!
//! A group's machine does not depend on which member was entered: the arms are the same
//! whichever one you called, and only the seed differs. So it is emitted once, as a
//! sibling of the members, and each member becomes an `#[inline]` call that seeds its own
//! entry. Without that, an *n*-member group would carry *n* copies of *n* arms.
//!
//! What makes it possible is a *seed* enum, one variant per member holding that member's
//! own parameters. Their types come from the signatures, so the shared function's
//! signature can be written, whereas the entry and frame payloads — a loop's state, a
//! resume point's locals — cannot be named at all and stay inside it, where inference
//! still reaches them.
//!
//! The shared items are named after every member of the group, as in
//! `__ss_machine_is_even_is_odd`, so that one container may hold several groups and an
//! expansion says which functions share which machine.
//!
//! A group of methods is lifted too. An enum cannot be declared inside an impl block, so
//! the seed is emitted beside it with `Self` replaced by the type the impl is for, which
//! is available exactly because it is an impl block. The machine stays inside as an
//! associated function, which is what keeps `Self` working in the arms untouched.
//!
//! A member that came out of a *body* — one declared inside another member and recursing
//! with it — keeps the name it had but is written inside the machine, beside the items
//! those bodies declared, since the body that declared it has become one of the machine's
//! own arms. Such a group therefore has to be lifted; there is no copy-per-member shape
//! for it, because a copy would have to contain the member that contains it.
//!
//! The seed carries the members' own generic parameters too — the union of them, keyed by
//! name — so a generic cycle shares one machine like any other, and so does one naming a
//! lifetime or passing a `&dyn Trait`.
//!
//! Some groups do get a copy per member, and `emit::liftable` says which: a group of one,
//! which has nothing to share; one whose parameters cannot be shared, because two members
//! ask genuinely different things of the same name — bounds are compared as sets, and a
//! where-clause counts as bounds — or because a parameter is used in no parameter type at
//! all; and one whose parameters the seed cannot spell at all: an `impl Trait`, or a `Self`
//! with no concrete type supplied.
//!
//! A parameter written as a bare path that hides a reference, such as an alias used as
//! `w: Words` rather than `w: Words<'_>`, is the one shape that is neither lifted nor
//! detected: the seed field needs a lifetime the tokens never mention, so it is an
//! `E0106` on that parameter. Writing the elision out fixes it.
//!
//! # What one group costs
//!
//! Calls *within* a group become entries into its driver and cost no native stack. A
//! call from one group to another is an ordinary call: the callee runs its own driver
//! to completion. Native depth is therefore bounded by the longest path between
//! groups, which is fixed at compile time.

use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use syn::spanned::Spanned;
use syn::{ImplItem, Item, ItemFn, ItemImpl, ItemMod};

use super::Opts;
use super::scan::{Scanned, Scope};

/// The functions a module holds. Their markers stay on: the scan reads them as it walks the
/// scope, which is what lets an option cover exactly what the body it was asked in declares.
pub(super) fn module_functions(module: &mut ItemMod) -> syn::Result<Vec<ItemFn>> {
    let Some((_, items)) = &module.content else {
        return Err(syn::Error::new(
            module.span(),
            "`#[stack_safe]` needs a module with a body: it has to see every function in \
             the module to know which of them recurse through each other",
        ));
    };
    items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(f) => Some(f.clone()),
            _ => None,
        })
        .map(Ok)
        .collect()
}

/// The same for an impl block, whose methods become plain functions of their receiver further
/// down. A `default fn` is rejected here, where the impl item still says so.
pub(super) fn impl_functions(block: &mut ItemImpl) -> syn::Result<Vec<ItemFn>> {
    block
        .items
        .iter()
        .filter_map(|item| match item {
            ImplItem::Fn(m) => Some(m.clone()),
            _ => None,
        })
        .map(|m| {
            if let Some(d) = m.modifiers.defaultness {
                return Err(syn::Error::new(
                    d.span(),
                    "`#[stack_safe]` does not support a `default fn` in an impl block",
                ));
            }
            Ok(ItemFn {
                attrs: m.attrs,
                vis: m.vis,
                modifiers: m.modifiers,
                sig: m.sig,
                block: Box::new(m.block),
            })
        })
        .collect()
}

/// Put the module back together: each rewritten function where it stood, every other item as it
/// was, nested containers descended into, and the module's own functions re-exported beside it.
///
/// Says also whether anything here was rewritten, nested containers included: an annotated module
/// that flattened nothing is a mistake, and only the caller knows which module was annotated.
pub(super) fn rebuild_mod(
    module: ItemMod,
    scanned: Scanned,
    opts: Opts,
    thread_out: bool,
) -> syn::Result<(TokenStream, bool)> {
    let ItemMod {
        attrs,
        vis,
        unsafety,
        ident,
        content,
        ..
    } = module;
    let (_, items) = content.expect("`module_functions` needed the body and found it");
    debug_assert!(
        scanned.hoisted.is_empty(),
        "a module hosts its own shared items",
    );

    // One answer per function, in order, so the items are walked and the next answer taken at
    // each function. Everything else comes out as it went in.
    let mut answers = scanned.rewritten.into_iter().zip(scanned.originals);
    let mut out_items: Vec<TokenStream> = Vec::with_capacity(items.len());
    let mut transformed = false;
    for item in &items {
        out_items.push(match item {
            Item::Fn(f) => match answers.next().expect("one answer per function") {
                (Some(tokens), original) => {
                    transformed = true;
                    quote! { #tokens #original }
                }
                (None, Some(original)) => {
                    // Not rewritten, but a cycle in its body was, so the copy still has something
                    // to hold that body to.
                    let mut f = f.clone();
                    f.attrs.retain(|a| !Opts::is_marker(a));
                    quote! { #f #original }
                }
                // Not rewritten, but the marker still has to go: the compiler would otherwise
                // run `#[stack_safe]` on it afterwards. Its shape was checked when the scope
                // handed its functions over, so dropping it is all that is left to do.
                (None, None) => {
                    let mut f = f.clone();
                    f.attrs.retain(|a| !Opts::is_marker(a));
                    f.to_token_stream()
                }
            },
            // A nested container is grouped on its own, through the same one entry — but only an
            // *annotated* module threads its functions out, since one reached by descending into
            // it is not at the caller's scope anyway.
            Item::Mod(inner) if inner.content.is_some() => {
                let (tokens, inner_transformed) =
                    Scope::of_mod(inner.clone())?.expand_reporting(opts, false)?;
                transformed |= inner_transformed;
                tokens
            }
            Item::Impl(inner) => {
                let (tokens, inner_transformed) =
                    Scope::of_impl(inner.clone())?.expand_reporting(opts, false)?;
                transformed |= inner_transformed;
                tokens
            }
            other => other.to_token_stream(),
        });
    }

    // Every top-level function the module lets out is re-exported beside it, so a caller at this
    // scope does not have to name the module. A `use` rather than a forwarding definition: it
    // never has to reproduce a signature, so a parameter pattern, a generic, or a type only the
    // module can name all come along for free.
    let reexports = items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(f) if thread_out && reaches_outside(&f.vis) => Some(f),
            _ => None,
        })
        .map(|f| {
            let vis = threaded_visibility(&f.vis, &vis);
            let name = &f.sig.ident;
            // A proc macro sees `#[cfg]` unexpanded, so without the condition the member would be
            // configured out while the `use` naming it stayed — an `E0432` on the module.
            let gates = f
                .attrs
                .iter()
                .filter(|a| a.path().is_ident("cfg") || a.path().is_ident("cfg_attr"));
            // Generated, so an unused one is noise rather than a finding.
            quote! {
                #(#gates)*
                #[allow(unused_imports)]
                #vis use #ident::#name;
            }
        });

    Ok((
        quote! {
            #(#attrs)*
            #vis #unsafety mod #ident {
                #(#out_items)*
            }
            #(#reexports)*
        },
        transformed,
    ))
}

/// Put the impl block back together. A method's body needs `Self` and the impl's generics, so it
/// is rewritten where it stands; only what cannot live in an impl block — a group's seed enum —
/// goes beside it.
///
/// Says also whether any method was rewritten, as `rebuild_mod` does. An impl block holds no nested
/// container, so its own methods are the whole answer.
pub(super) fn rebuild_impl(block: ItemImpl, scanned: Scanned) -> syn::Result<(TokenStream, bool)> {
    let hoisted = scanned.hoisted;
    let mut transformed = false;
    // A copy kept for checking is not a member of the trait, and a trait impl may hold nothing
    // else, so a trait impl goes unchecked. An inherent copy beside it is no answer either: the
    // self type may belong to another crate, where no inherent impl is allowed.
    let checked = block.trait_.is_none();
    let mut answers = scanned.rewritten.into_iter().zip(scanned.originals);
    let mut out_items: Vec<TokenStream> = Vec::with_capacity(block.items.len());
    for item in &block.items {
        out_items.push(match item {
            ImplItem::Fn(m) => {
                let (rewritten, original) = answers.next().expect("one answer per function");
                let original = original.filter(|_| checked);
                match rewritten {
                    Some(tokens) => {
                        transformed = true;
                        quote! { #tokens #original }
                    }
                    None => {
                        // Not rewritten, but its marker still has to go.
                        let mut m = m.clone();
                        m.attrs.retain(|a| !Opts::is_marker(a));
                        quote! { #m #original }
                    }
                }
            }
            other => other.to_token_stream(),
        });
    }

    let ItemImpl {
        attrs,
        modifiers,
        unsafety,
        generics,
        trait_,
        self_ty,
        ..
    } = &block;
    let (impl_generics, _, where_clause) = generics.split_for_impl();
    let (defaultness, polarity) = (&modifiers.defaultness, &modifiers.polarity);
    let trait_for = trait_
        .as_ref()
        .map(|(path, for_token)| quote! { #polarity #path #for_token });
    Ok((
        quote! {
            #(#hoisted)*

            #(#attrs)*
            #defaultness #unsafety impl #impl_generics #trait_for #self_ty #where_clause {
                #(#out_items)*
            }
        },
        transformed,
    ))
}

/// Can a caller *outside* the module reach this function? One the module keeps to
/// itself has no name to carry out.
fn reaches_outside(vis: &syn::Visibility) -> bool {
    match vis {
        syn::Visibility::Public(_) => true,
        // `pub(crate)` and `pub(super)` both reach the module's parent, which is
        // where the name is carried to. `pub(self)` does not, and `pub(in path)`
        // might name the module itself, which cannot be told apart from an ancestor
        // without resolving the path — so it is left alone.
        syn::Visibility::Restricted(r) => r.path.is_ident("crate") || r.path.is_ident("super"),
        syn::Visibility::Inherited => false,
    }
}

/// The visibility a threaded-out name carries at the module's own scope.
///
/// Two corrections to copying the tokens across. A function's `pub(super)` meant
/// "visible to the module's parent" — but the copy *is* in that parent, where the
/// same reach is spelled by no keyword at all. And the copy can be no more visible
/// than the module it came from: a `pub fn` inside a private module was never
/// reachable from outside, and threading it out must not change that.
fn threaded_visibility(func: &syn::Visibility, module: &syn::Visibility) -> TokenStream {
    /// How far a visibility reaches, counted from the module's parent: 3 anywhere,
    /// 2 the crate, 1 the grandparent, 0 just here.
    fn reach(vis: &syn::Visibility, shift_super: bool) -> u8 {
        match vis {
            syn::Visibility::Public(_) => 3,
            syn::Visibility::Restricted(r) if r.path.is_ident("crate") => 2,
            // Written inside the module, `super` names the parent — which is where
            // the copy lives, so from there it is simply private.
            syn::Visibility::Restricted(r) if r.path.is_ident("super") => {
                if shift_super {
                    0
                } else {
                    1
                }
            }
            _ => 0,
        }
    }

    match reach(func, true).min(reach(module, false)) {
        3 => quote! { pub },
        2 => quote! { pub(crate) },
        1 => quote! { pub(super) },
        _ => TokenStream::new(),
    }
}
