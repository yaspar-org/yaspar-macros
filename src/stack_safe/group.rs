// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! `#[stack_safe]` on a module or impl block: find the functions that recurse, alone or
//! through each other, and give each cycle one shared driver.
//!
//! Mutual recursion needs the attribute on the container, since expanding `f` needs `g`'s
//! body. Nested modules and impl blocks are grouped separately; cycles crossing containers
//! are not detected. Cycles are found by transitive closure over the call graph (`scope.rs`),
//! including functions declared inside bodies.
//!
//! A multi-member group is emitted once, as a shared machine entered through a *seed* enum
//! (one variant per member's parameters); each member becomes an `#[inline]` call seeding its
//! entry. For methods, the seed goes beside the impl with `Self` replaced. `emit::liftable`
//! decides when a group instead gets a copy per member.
//!
//! An annotated module's reachable top-level functions are re-exported beside it with `use`.
//!
//! Calls between groups are ordinary calls, so native depth is bounded by the longest path
//! between groups.

use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use syn::spanned::Spanned;
use syn::{ImplItem, Item, ItemFn, ItemImpl, ItemMod};

use super::Opts;
use super::scan::{Scanned, Scope};

/// The module's functions, markers kept so the scan can scope their options.
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

/// The impl block's methods as `ItemFn`s. Rejects `default fn`.
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

/// Rebuild the module with rewritten functions, expanded nested containers, and re-exports.
/// Also returns whether anything (nested included) was rewritten.
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

    // One answer per function, in item order.
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
                    // A cycle in its body was rewritten.
                    let mut f = f.clone();
                    f.attrs.retain(|a| !Opts::is_marker(a));
                    quote! { #f #original }
                }
                // Drop the marker so the compiler doesn't expand it again.
                (None, None) => {
                    let mut f = f.clone();
                    f.attrs.retain(|a| !Opts::is_marker(a));
                    f.to_token_stream()
                }
            },
            // Nested containers are grouped separately and never thread out.
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

    // Re-export with `use` rather than a forwarder, so no signature has to be reproduced.
    let reexports = items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(f) if thread_out && reaches_outside(&f.vis) => Some(f),
            _ => None,
        })
        .map(|f| {
            let vis = threaded_visibility(&f.vis, &vis);
            let name = &f.sig.ident;
            // Copy `#[cfg]`s, or a configured-out function leaves a dangling `use` (E0432).
            let gates = f
                .attrs
                .iter()
                .filter(|a| a.path().is_ident("cfg") || a.path().is_ident("cfg_attr"));
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

/// Rebuild the impl block with methods rewritten in place and seed enums hoisted beside it.
/// Also returns whether any method was rewritten.
pub(super) fn rebuild_impl(block: ItemImpl, scanned: Scanned) -> syn::Result<(TokenStream, bool)> {
    let hoisted = scanned.hoisted;
    let mut transformed = false;
    // A trait impl can't hold the checking copy (not a trait member), nor can an inherent impl
    // (the self type may be foreign), so trait impls go unchecked.
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
                        // Drop the marker.
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

/// Whether the function is visible from the module's parent.
fn reaches_outside(vis: &syn::Visibility) -> bool {
    match vis {
        syn::Visibility::Public(_) => true,
        // `pub(in path)` can't be resolved here, so it is skipped.
        syn::Visibility::Restricted(r) => r.path.is_ident("crate") || r.path.is_ident("super"),
        syn::Visibility::Inherited => false,
    }
}

/// Visibility of a re-export in the module's parent: the narrower of the function's and the
/// module's, with the function's `pub(super)` becoming private.
fn threaded_visibility(func: &syn::Visibility, module: &syn::Visibility) -> TokenStream {
    /// Reach from the module's parent: 3 anywhere, 2 crate, 1 grandparent, 0 here.
    fn reach(vis: &syn::Visibility, shift_super: bool) -> u8 {
        match vis {
            syn::Visibility::Public(_) => 3,
            syn::Visibility::Restricted(r) if r.path.is_ident("crate") => 2,
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
