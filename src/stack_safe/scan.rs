// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Scanning a scope and rewriting what recurses in it.
//!
//! The one entry for both kinds of item the attribute goes on. A single function is a scope of
//! one *root*; a module or an impl block is a scope of as many roots as it has functions. Either
//! way the roots and everything their bodies declare go into one graph ([`scope`]), the cycles
//! are read off it, and each is handed to [`emit::expand_group`] to be given a driver.
//!
//! # Where a cycle's driver goes
//!
//! Where its outermost member was declared. That one rule covers every case:
//!
//! - a cycle among the roots — the usual mutual recursion — is written where the roots are, so
//!   [`expand_roots`] hands each member's rewritten form back to whoever owns them;
//! - a cycle declared entirely inside one body is written back into that body, since nothing
//!   outside it could name its members anyway;
//! - either way, a member declared *deeper* than the outermost one is written **inside** the
//!   driver. The body that declared it has become one of the driver's own arms, so there is
//!   nowhere else for it to go — and it keeps the name it had, because a name scoped to a body
//!   is nobody else's, so a helper that called it there still finds it.
//!
//! Innermost cycle first, so that a member is only taken out of a body once everything declared
//! inside it is finished.

use proc_macro2::{Ident, TokenStream};
use quote::{ToTokens, quote};
use std::collections::HashMap;
use syn::{FnArg, ItemFn, ItemImpl, ItemMod};

use super::Opts;
use super::analyze::rename_calls;
use super::emit::expand_group;
use super::names;
use super::{group, scope};

/// The scope handed to the scan.
struct Roots<'a> {
    /// The functions to scan. Their own `#[stack_safe]` markers are still on them: reading those
    /// is part of walking the scope, and the walk is what hands the options down.
    funcs: Vec<ItemFn>,
    /// The options the attribute itself was given, which everything in the scope starts from.
    opts: Opts,
    /// The type an impl block is for, which its methods' arms need in order to name `Self`.
    self_ty: Option<&'a syn::Type>,
    /// Are they associated items? See [`scope::edges`].
    assoc: bool,
    /// Are they the members of a *trait* impl? Such a block may hold nothing but the trait's own
    /// members, which is room enough for a rewritten body's entry point but not for what has to sit
    /// beside it.
    trait_impl: bool,
}

/// What rewriting a scope produced.
pub(super) struct Scanned {
    /// Per root, what to emit in its place — or `None` for one left exactly as written, which
    /// neither recursed nor had a cycle rewritten inside its body.
    pub(super) rewritten: Vec<Option<TokenStream>>,
    /// Per root, a copy of it as the user wrote it, named `<name>_orig`, for a cycle that opted
    /// into one of the unsafe options. Nothing calls it. It is there to be *checked*: those
    /// options hand the driver raw pointers where the original had references, so the borrow
    /// checker stops seeing what the original asked of it, and a program it would have refused
    /// compiles. The copy gives it back the original to refuse. See [`originals`].
    pub(super) originals: Vec<Option<TokenStream>>,
    /// What a cycle needs written *beside* the container rather than inside it. An impl block
    /// cannot hold an enum, so a group of methods puts its seed enum here.
    pub(super) hoisted: Vec<TokenStream>,
}

/// A `#[stack_safe]` target, parsed: the functions to scan, and what they came out of.
///
/// A function is a scope of one root. A module or an impl block is a scope of as many roots as
/// it has functions, which is what mutual recursion needs — rewriting `f` takes `g`'s body — and
/// is why one attribute serves all three.
pub(super) struct Scope {
    /// The functions to scan, markers and all.
    funcs: Vec<ItemFn>,
    host: Host,
}

/// The scope with its functions taken out: enough to say what a name in it can mean, and enough
/// to put it back together afterwards.
enum Host {
    /// A lone function keeps only its name, since what replaces it is the scan's answer and the
    /// name is for the error when nothing in its scope recurses at all.
    Fn {
        name: Ident,
        assoc: bool,
    },
    Mod(ItemMod),
    Impl(ItemImpl),
}

impl Scope {
    /// The annotated item.
    pub(super) fn parse(item: TokenStream) -> syn::Result<Self> {
        if let Ok(func) = syn::parse2::<ItemFn>(item.clone()) {
            // A receiver makes it an associated item, and then a bare name in its body means a
            // free function rather than a call to it. An associated function *without* a
            // receiver cannot be told from a free one: this attribute sees the function, not
            // what holds it.
            let host = Host::Fn {
                name: func.sig.ident.clone(),
                assoc: matches!(func.sig.inputs.first(), Some(FnArg::Receiver(_))),
            };
            return Ok(Scope {
                funcs: vec![func],
                host,
            });
        }
        if let Ok(module) = syn::parse2::<ItemMod>(item.clone()) {
            return Scope::of_mod(module);
        }
        let block = syn::parse2::<ItemImpl>(item).map_err(|e| {
            syn::Error::new(
                e.span(),
                "`#[stack_safe]` applies to a function, a module or an impl block: it has to \
                 see a body to know what recurses, and this item has none",
            )
        })?;
        Scope::of_impl(block)
    }

    /// A container reached by descending into one, which is grouped on its own.
    pub(super) fn of_mod(module: ItemMod) -> syn::Result<Self> {
        let mut module = module;
        Ok(Scope {
            funcs: group::module_functions(&mut module)?,
            host: Host::Mod(module),
        })
    }

    pub(super) fn of_impl(block: ItemImpl) -> syn::Result<Self> {
        let mut block = block;
        Ok(Scope {
            funcs: group::impl_functions(&mut block)?,
            host: Host::Impl(block),
        })
    }

    /// Rewrite every recursion in this scope.
    ///
    /// The one flow, whichever item the attribute sits on: hand the functions to the scan, then
    /// put the scope back together from what comes out. `thread_out` is for the one thing only an
    /// *annotated* module does — re-export its functions beside itself — since a module reached
    /// by descending into it is not at the caller's scope anyway.
    pub(super) fn expand(self, opts: Opts, thread_out: bool) -> syn::Result<TokenStream> {
        let Scope { funcs, host } = self;
        let scanned = expand_roots(Roots {
            funcs,
            opts,
            self_ty: host.self_ty(),
            assoc: host.assoc(),
            trait_impl: host.trait_impl(),
        })?;
        host.rebuild(scanned, opts, thread_out)
    }
}

impl Host {
    /// Are the roots associated items? That decides what a name can mean: `self.g(..)` and
    /// `Self::g(..)` reach them and a bare `g(..)` does not.
    fn assoc(&self) -> bool {
        match self {
            Host::Fn { assoc, .. } => *assoc,
            Host::Mod(_) => false,
            Host::Impl(_) => true,
        }
    }

    /// Is this a trait impl, which may hold nothing but the trait's own members?
    fn trait_impl(&self) -> bool {
        matches!(self, Host::Impl(block) if block.trait_.is_some())
    }

    /// The type an impl block is for, which a group of its methods needs in order to declare its
    /// seed enum beside the block, where `Self` means nothing. With generics on the impl the seed
    /// would have to carry those too, so such a group is left unlifted.
    fn self_ty(&self) -> Option<&syn::Type> {
        match self {
            Host::Impl(block) if block.generics.params.is_empty() => Some(&block.self_ty),
            _ => None,
        }
    }

    fn rebuild(self, scanned: Scanned, opts: Opts, thread_out: bool) -> syn::Result<TokenStream> {
        match self {
            Host::Fn { name, .. } => {
                let hoisted = scanned.hoisted;
                let original = scanned.originals.into_iter().next().expect("one root");
                match scanned.rewritten.into_iter().next().expect("one root") {
                    Some(tokens) => Ok(quote! { #(#hoisted)* #tokens #original }),
                    None => Err(syn::Error::new(
                        name.span(),
                        format!(
                            "`#[stack_safe]` on `{name}` has no effect: nothing in its scope \
                             recurses, so there is no recursion to flatten. `{name}` itself \
                             never calls `{name}`, and neither it nor any function declared in \
                             its body is part of a cycle. A function that recurses only \
                             *through* one declared elsewhere has to be scanned together with \
                             it, so put `#[stack_safe]` on the enclosing module or impl block"
                        ),
                    )),
                }
            }
            Host::Mod(module) => group::rebuild_mod(module, scanned, opts, thread_out),
            Host::Impl(block) => group::rebuild_impl(block, scanned),
        }
    }
}

/// Rewrite every recursion among these functions and inside their bodies.
///
/// `opts` is one set of options per root, as its own `#[stack_safe(..)]` marker gave them; a
/// cycle obeys every option any of its members asked for. `self_ty` is the type an impl block
/// is for, which its methods' arms need in order to name `Self`. `assoc` says the roots are
/// associated items, which decides what a name can mean — see [`scope::edges`].
/// A copy of each root that a cycle under an unsafe option touches, as the user wrote it, for the
/// borrow checker to hold to the original's terms.
///
/// The copies call each other rather than the rewritten functions, so what is checked is the
/// original program and not a mixture of the two. A name declared more than once in the scope is
/// left alone: which function such a call means is a question of scope, and answering it wrongly
/// here would report an error against a program the user did not write. The copy is private,
/// carries the original's own attributes, and is never called.
fn originals(
    as_written: &[ItemFn],
    defs: &[scope::Def],
    wants_check: &[bool],
) -> Vec<Option<TokenStream>> {
    let renames: HashMap<String, Ident> = as_written
        .iter()
        .enumerate()
        .filter(|&(i, _)| wants_check[i])
        .map(|(_, func)| &func.sig.ident)
        .filter(|name| defs.iter().filter(|d| &d.name == *name).count() == 1)
        .map(|name| (name.to_string(), names::original(name)))
        .collect();

    as_written
        .iter()
        .enumerate()
        .map(|(i, func)| {
            if !wants_check[i] {
                return None;
            }
            let mut copy = func.clone();
            copy.vis = syn::Visibility::Inherited;
            copy.sig.ident = names::original(&func.sig.ident);
            rename_calls(&mut copy, &renames);
            // Nothing calls it, and whatever the original is warned about is already reported
            // against the original. Only the hard errors are wanted here.
            Some(quote! {
                #[allow(warnings)]
                #copy
            })
        })
        .collect()
}

/// The options a cycle is transformed under.
///
/// Its members share one driver, so they have to agree on what that driver may do: one set of
/// options, not a union of what each asked for separately. A disagreement is reported against the
/// member that differs from the one the cycle is written with.
fn agreed_opts(defs: &[scope::Def], cycle: &[usize]) -> syn::Result<Opts> {
    let (&host, rest) = cycle.split_first().expect("a cycle has a member");
    let opts = defs[host].opts;
    for &member in rest {
        if defs[member].opts != opts {
            return Err(syn::Error::new(
                defs[member].name.span(),
                format!(
                    "`{}` and `{}` are mutually recursive, so they share one driver and must be \
                     given the same options; `{}` has [{}] and `{}` has [{}]",
                    defs[host].name,
                    defs[member].name,
                    defs[host].name,
                    opts.flags(),
                    defs[member].name,
                    defs[member].opts.flags(),
                ),
            ));
        }
    }
    Ok(opts)
}

fn expand_roots(roots: Roots<'_>) -> syn::Result<Scanned> {
    let Roots {
        funcs,
        opts: scope_opts,
        self_ty,
        assoc,
        trait_impl,
    } = roots;
    // The scope in declaration order, with each definition's marker taken off and the options in
    // force at it handed down from its host.
    let mut roots = funcs;
    let defs = scope::collect(&mut roots, scope_opts)?;

    let reaches = scope::closure(&scope::edges(&roots, &defs, assoc));
    for (i, d) in defs.iter().enumerate() {
        // A marker that turns out to cover no recursion is a mistake, wherever it was written.
        if d.marked && !reaches[i][i] {
            let nested = !d.path.is_empty();
            return Err(syn::Error::new(
                d.name.span(),
                format!(
                    "`#[stack_safe]` on `{}` has no effect: `#[stack_safe]` found no path from \
                     it back to itself, so it does not recurse.{}",
                    d.name,
                    if nested {
                        " It needs no attribute of its own in any case: the one covering the \
                         body it is declared in already applies to it"
                    } else {
                        ""
                    },
                ),
            ));
        }
    }

    // Kept as the user wrote them, before a cycle takes its members out or a body is rewritten in
    // place. Only worth the copies when something in the scope asks for an unsafe option.
    let checkable = defs
        .iter()
        .any(|d| d.opts.use_nonlinear_mut || d.opts.data_in_frame);
    let as_written: Vec<ItemFn> = if checkable {
        roots.to_vec()
    } else {
        Vec::new()
    };
    let mut wants_check = vec![false; roots.len()];

    let mut out = Scanned {
        rewritten: vec![None; roots.len()],
        originals: vec![None; roots.len()],
        hoisted: Vec::new(),
    };
    let mut changed = vec![false; roots.len()];

    for cycle in scope::cycles(&defs, &reaches) {
        // The outermost member, which is where this cycle is written. `cycle` comes back in
        // `defs` order, which is shallowest first, so it is the first of them.
        let host = &defs[cycle[0]];
        let depth = host.path.len();
        let inner: Vec<bool> = cycle.iter().map(|&j| defs[j].path.len() > depth).collect();

        // Every member, taken out of the body that declares it. Deepest first — `cycle`
        // reversed, by the same ordering — so that a member nested in another comes out before
        // the one holding it, and what comes out of the outer one no longer holds it. A root is
        // not nested in anything, so it is copied rather than taken; the copy has to be made
        // after its own members are out of its body.
        let mut members: Vec<ItemFn> = cycle
            .iter()
            .rev()
            .map(|&j| {
                let d = &defs[j];
                if d.path.is_empty() {
                    roots[d.owner].clone()
                } else {
                    scope::take(&mut roots[d.owner], &d.path)
                }
            })
            .collect();
        // Taken deepest first, but handed over in cycle order: the driver's declarations ride
        // with the outermost member, and `inner` says of each member which it is.
        members.reverse();

        // A rewritten member needs more beside it than the trait's own members: a method is split
        // into the signature the trait asked for and a plain associated function carrying the body,
        // since a body that becomes a driver cannot name `Self` from a nested `fn`. A trait impl has
        // no room for that. A function declared *in* such a body is another matter: its cycle's
        // driver is written in the body, where nothing is a member of anything.
        if trait_impl && depth == 0 {
            let member = &defs[cycle[0]];
            return Err(syn::Error::new(
                member.name.span(),
                format!(
                    "`{}` recurses, and `#[stack_safe]` cannot rewrite a recursive member of a \
                     trait impl: the rewritten body has to sit beside the member, and a trait impl \
                     may hold nothing but the trait's own members. Move the body to an inherent \
                     method, annotate that, and have this one forward to it. A function declared \
                     inside the body may still recurse, since its driver is written there",
                    member.name,
                ),
            ));
        }

        let cycle_opts = agreed_opts(&defs, &cycle)?;
        if cycle_opts.use_nonlinear_mut || cycle_opts.data_in_frame {
            // Every root this cycle touches: the members themselves for a cycle among the roots,
            // and the one hosting them for a cycle declared inside a body.
            for &j in &cycle {
                wants_check[defs[j].owner] = true;
            }
        }
        // `Self` is the impl block's, so it is nameable in an arm only for a cycle written
        // inside that block — which is to say one whose members are its functions.
        let self_ty = if depth == 0 { self_ty } else { None };
        let (entries, hoisted) = expand_group(members, cycle_opts, self_ty, &inner, assoc)?;

        if depth == 0 {
            // Each member is a root, keeping its own place; the driver goes with the first.
            for (&j, entry) in cycle.iter().zip(entries) {
                if defs[j].path.is_empty() {
                    out.rewritten[defs[j].owner] = Some(entry);
                }
            }
            if !hoisted.is_empty() {
                out.hoisted.push(hoisted);
            }
        } else {
            // No root is a member, so every member came out of one body — a cycle reaching a
            // function declared in a body runs through the function declaring it — and the
            // whole cycle goes back where the outermost of them was.
            debug_assert!(
                cycle.iter().all(|&j| defs[j].owner == host.owner),
                "a cycle with no root among its members lies inside one root",
            );
            let root = &mut roots[host.owner];
            scope::put_back(root, &host.path, quote! { #hoisted #(#entries)* });
            changed[host.owner] = true;
        }
    }

    // A root that is in no cycle but whose body held one is emitted as it now stands.
    for (i, root) in roots.iter().enumerate() {
        if out.rewritten[i].is_none() && changed[i] {
            out.rewritten[i] = Some(root.to_token_stream());
        }
    }
    if checkable {
        // Left as it was initialised otherwise: one `None` per root, since nothing asked to be
        // checked and `as_written` holds nothing to check.
        out.originals = originals(&as_written, &defs, &wants_check);
    }
    Ok(out)
}
