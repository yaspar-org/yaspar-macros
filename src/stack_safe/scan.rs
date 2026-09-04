// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Scanning a scope and rewriting what recurses in it.
//!
//! The one entry for both kinds of item the attribute goes on. A single function is a scope of
//! one *root*; a module or an impl block is a scope of as many roots as it has functions. Either
//! way the roots and everything their bodies declare go into one graph ([`scope`]), the cycles
//! are read off it, and each is handed to [`expand_group`] to be given a driver.
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
use syn::spanned::Spanned;
use syn::visit_mut::VisitMut;
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
    /// What the scope is called where it is written: the impl block's own type, or the annotated
    /// module's ident. A call may name a member through it rather than through `Self` or `self`.
    /// See [`scope::edges`].
    host_name: Option<Ident>,
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
    /// A marker on the container itself, if it carried one — a nested `#[stack_safe(..)] mod m`.
    /// Options are scoped like bindings, so this *shadows* whatever the enclosing attribute asked
    /// for, throughout this scope. Taking it off is not optional either: left in place the compiler
    /// would run the attribute again, on a body already rewritten.
    own_opts: Option<Opts>,
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
                own_opts: None,
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
        let own_opts = Opts::take_from(&mut module.attrs)?;
        Ok(Scope {
            funcs: group::module_functions(&mut module)?,
            host: Host::Mod(module),
            own_opts,
        })
    }

    pub(super) fn of_impl(block: ItemImpl) -> syn::Result<Self> {
        let mut block = block;
        let own_opts = Opts::take_from(&mut block.attrs)?;
        Ok(Scope {
            funcs: group::impl_functions(&mut block)?,
            host: Host::Impl(block),
            own_opts,
        })
    }

    /// Rewrite every recursion in this scope.
    ///
    /// The one flow, whichever item the attribute sits on: hand the functions to the scan, then
    /// put the scope back together from what comes out. `thread_out` is for the one thing only an
    /// *annotated* module does — re-export its functions beside itself — since a module reached
    /// by descending into it is not at the caller's scope anyway.
    pub(super) fn expand(self, opts: Opts, thread_out: bool) -> syn::Result<TokenStream> {
        Ok(self.expand_reporting(opts, thread_out)?.0)
    }

    /// The scope the attribute was written on, which owes an answer for itself.
    ///
    /// A function that turns out to recurse nowhere is already an error, reported by name. A
    /// container was not, so `#[stack_safe] mod m` over a module whose functions merely call one
    /// another in a line compiled with no diagnostic at all — and a refactor that broke a module's
    /// recursion was undetectable. It is the same mistake either way, so it now reads the same way.
    pub(super) fn expand_annotated(self, opts: Opts) -> syn::Result<TokenStream> {
        let complaint = self.host.no_effect();
        let (tokens, transformed) = self.expand_reporting(opts, true)?;
        match complaint {
            Some(err) if !transformed => Err(err),
            _ => Ok(tokens),
        }
    }

    /// The same, saying also whether anything in this scope was rewritten.
    ///
    /// Which is what an annotated container has to know: one that flattened nothing is a mistake,
    /// and a container reached by descending into it may have been the one that did the flattening.
    pub(super) fn expand_reporting(
        self,
        opts: Opts,
        thread_out: bool,
    ) -> syn::Result<(TokenStream, bool)> {
        let Scope {
            funcs,
            host,
            own_opts,
        } = self;
        // A marker on the container shadows the enclosing attribute's options for this scope, the
        // same rule a marker on a function follows.
        let opts = own_opts.unwrap_or(opts);
        let scanned = expand_roots(Roots {
            funcs,
            opts,
            self_ty: host.self_ty(),
            host_name: host.host_name(),
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

    /// The name a call may reach into this scope by, other than `Self` or `self`: the leading
    /// segment of the type an impl block is for, or the module's own ident. Generics do not matter
    /// here, unlike for [`Host::self_ty`] — this is a name to compare a path against, not a type to
    /// write down.
    ///
    /// A lone function has none. Nothing names its body from outside, and the function itself is
    /// reached by the name it was declared with, which is the bare form already.
    fn host_name(&self) -> Option<Ident> {
        match self {
            Host::Fn { .. } => None,
            Host::Mod(module) => Some(module.ident.clone()),
            Host::Impl(block) => match &*block.self_ty {
                syn::Type::Path(p) => p.path.segments.first().map(|s| s.ident.clone()),
                _ => None,
            },
        }
    }

    /// What to say if nothing in this scope turned out to recurse.
    ///
    /// `None` for a function, which reports that for itself, in its own words, from `rebuild`.
    fn no_effect(&self) -> Option<syn::Error> {
        let (kind, name, span) = match self {
            Host::Fn { .. } => return None,
            Host::Mod(module) => ("module", module.ident.to_string(), module.ident.span()),
            Host::Impl(block) => {
                let ty = &*block.self_ty;
                ("impl block", quote! { #ty }.to_string(), ty.span())
            }
        };
        Some(syn::Error::new(
            span,
            format!(
                "`#[stack_safe]` on this {kind} has no effect: nothing in `{name}` recurses, so \
                 there is no recursion to flatten. Every function it holds — and every one their \
                 bodies declare, and every nested module and impl block — was scanned, and none \
                 of them reaches itself. A cycle that leaves this scope cannot be seen from here, \
                 so if these functions recurse through one declared elsewhere, the attribute \
                 belongs on the scope that holds them all"
            ),
        ))
    }

    fn rebuild(
        self,
        scanned: Scanned,
        opts: Opts,
        thread_out: bool,
    ) -> syn::Result<(TokenStream, bool)> {
        match self {
            Host::Fn { name, .. } => {
                let hoisted = scanned.hoisted;
                let original = scanned.originals.into_iter().next().expect("one root");
                match scanned.rewritten.into_iter().next().expect("one root") {
                    Some(tokens) => Ok((quote! { #(#hoisted)* #tokens #original }, true)),
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

/// Expand every module and impl block declared in this body, deepest scope first, and report
/// whether there was one.
///
/// A body is a scope of item definitions like any other, so it may hold a container, and a
/// recursion inside that container is exactly as invisible to the native stack as one anywhere
/// else. It is handed to the same one entry a container declared in a module is, and what comes
/// back is written where it stood. `thread_out` is false: nothing outside the body could name the
/// container's functions anyway.
///
/// A marker of its own says what that subtree wants, in full, exactly as one on a `fn` does.
fn expand_nested_containers(func: &mut ItemFn, opts: Opts) -> syn::Result<bool> {
    struct V {
        opts: Opts,
        found: bool,
        failed: Option<syn::Error>,
    }

    impl V {
        /// The tokens a nested container expands to, or `None` for an item that is not one.
        fn expanded(&mut self, item: &syn::Item) -> syn::Result<Option<TokenStream>> {
            match item {
                // A module with no body is rejected by the compiler before the tokens ever reach
                // here: a file module in proc-macro input is unstable.
                syn::Item::Mod(inner) if inner.content.is_some() => {
                    let mut inner = inner.clone();
                    let own = Opts::take_from(&mut inner.attrs)?;
                    Scope::of_mod(inner)?
                        .expand(own.unwrap_or(self.opts), false)
                        .map(Some)
                }
                syn::Item::Impl(inner) => {
                    let mut inner = inner.clone();
                    let own = Opts::take_from(&mut inner.attrs)?;
                    Scope::of_impl(inner)?
                        .expand(own.unwrap_or(self.opts), false)
                        .map(Some)
                }
                _ => Ok(None),
            }
        }
    }

    impl VisitMut for V {
        fn visit_item_mut(&mut self, item: &mut syn::Item) {
            if let syn::Item::Fn(inner) = item {
                // A function declared here is a scope of its own, and its body may hold a container
                // too.
                self.visit_block_mut(&mut inner.block);
                return;
            }
            match self.expanded(item) {
                // The first failure wins, as it would if this returned a `Result`.
                Err(e) => self.failed = self.failed.take().or(Some(e)),
                Ok(None) => {}
                Ok(Some(tokens)) => {
                    self.found = true;
                    *item = syn::Item::Verbatim(tokens);
                }
            }
        }
    }

    let mut v = V {
        opts,
        found: false,
        failed: None,
    };
    v.visit_block_mut(&mut func.block);
    match v.failed {
        Some(e) => Err(e),
        None => Ok(v.found),
    }
}

/// Refuse a recursion the scan can see and the transform cannot rewrite.
///
/// A call written through a path that says what it names but is not one of the shapes the rewriter
/// follows — `T::g(..)` inside `impl T`, `<Self>::g(..)`, `crate::m::g(..)` inside `#[stack_safe]
/// mod m` — cannot become an entry into a driver. Treating it as one anyway would emit a function
/// whose other calls are flattened and whose this one still descends natively, so it is reported
/// instead, and only where it matters: such a call closes a cycle nothing else in the graph closes.
/// One that merely reaches a function of this scope, recursion or no recursion, is an ordinary call
/// and stays one.
fn unresolvable_recursion(
    defs: &[scope::Def],
    edges: &[Vec<bool>],
    reaches: &[Vec<bool>],
    blocked: &[scope::Blocked],
    assoc: bool,
) -> syn::Result<()> {
    if blocked.is_empty() {
        return Ok(());
    }
    // The graph as it would be if every one of these calls were an edge, which is what says whether
    // one of them is what makes a cycle a cycle.
    let mut optimistic = edges.to_vec();
    for b in blocked {
        optimistic[b.caller][b.callee] = true;
    }
    let optimistic = scope::closure(&optimistic);

    for b in blocked {
        if !optimistic[b.callee][b.caller] || reaches[b.caller][b.callee] {
            continue;
        }
        let callee = &defs[b.callee].name;
        let forms = match assoc {
            true => format!("`Self::{callee}(..)` or `self.{callee}(..)`"),
            false => format!("`{callee}(..)` or `self::{callee}(..)`"),
        };
        return Err(syn::Error::new(
            b.span,
            format!(
                "`{callee}` is called through the path `{}`, which `#[stack_safe]` cannot rewrite: \
                 a macro resolves no paths, so it recognises a call to something in its scope only \
                 by the shape of it, and this call is part of a cycle it would otherwise have to \
                 leave on the native stack. Write it as {forms}",
                b.path,
            ),
        ));
    }
    Ok(())
}

fn expand_roots(roots: Roots<'_>) -> syn::Result<Scanned> {
    let Roots {
        funcs,
        opts: scope_opts,
        self_ty,
        host_name,
        assoc,
        trait_impl,
    } = roots;
    let mut roots = funcs;
    // A container declared in a body is a scope of its own, so it is expanded before anything else
    // looks at the body: what comes back is tokens, and the scan has nothing more to do there.
    let mut changed = vec![false; roots.len()];
    for (i, root) in roots.iter_mut().enumerate() {
        changed[i] = expand_nested_containers(root, scope_opts)?;
    }
    // The scope in declaration order, with each definition's marker taken off and the options in
    // force at it handed down from its host.
    let defs = scope::collect(&mut roots, scope_opts)?;

    let (edges, blocked) = scope::edges(&roots, &defs, assoc, host_name.as_ref());
    let reaches = scope::closure(&edges);
    unresolvable_recursion(&defs, &edges, &reaches, &blocked, assoc)?;
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
