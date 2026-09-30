// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Scan a scope (a function, module or impl block) and rewrite what recurses in it.
//!
//! All functions and everything their bodies declare go into one graph ([`scope`]); each
//! cycle is handed to [`expand_group`]. A cycle's driver is written where its outermost
//! member was declared; deeper members go inside the driver, keeping their names.
//! Innermost cycles are processed first.

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
use super::try_shim;
use super::{group, scope};

/// The scope handed to the scan.
struct Roots<'a> {
    /// The functions to scan, with their own `#[stack_safe]` markers still on.
    funcs: Vec<ItemFn>,
    /// The attribute's options, the default for the whole scope.
    opts: Opts,
    /// The impl block's self type, so methods' arms can name `Self`.
    self_ty: Option<&'a syn::Type>,
    /// The impl's type or the module's ident, which a call may name a member through.
    /// See [`scope::edges`].
    host_name: Option<Ident>,
    /// Whether the roots are associated items. See [`scope::edges`].
    assoc: bool,
    /// Whether this is a trait impl, which cannot hold the extra items a rewrite needs.
    trait_impl: bool,
}

/// What rewriting a scope produced.
pub(super) struct Scanned {
    /// Per root, its replacement, or `None` if left as written.
    pub(super) rewritten: Vec<Option<TokenStream>>,
    /// Per root, an uncalled copy named `<name><original_suffix>`, emitted for every root a cycle
    /// touches so the compiler still checks the original recursion. See [`originals`].
    pub(super) originals: Vec<Option<ItemFn>>,
    /// Items to emit beside the container, e.g. a group of methods' seed enum (an impl block
    /// cannot hold an enum).
    pub(super) hoisted: Vec<TokenStream>,
}

/// A parsed `#[stack_safe]` target: a function (one root), or a module or impl block (one
/// root per function).
pub(super) struct Scope {
    funcs: Vec<ItemFn>,
    host: Host,
    /// Options from a marker on the container itself (a nested `#[stack_safe(..)] mod m`),
    /// shadowing the enclosing ones. Always removed so the attribute does not run again.
    own_opts: Option<Opts>,
}

/// The scope with its functions taken out, for resolving names and rebuilding it.
enum Host {
    /// A lone function keeps only its name, for the no-effect error.
    Fn {
        name: Ident,
        assoc: bool,
    },
    Mod(ItemMod),
    Impl(ItemImpl),
}

impl Scope {
    /// Parse the annotated item.
    pub(super) fn parse(item: TokenStream) -> syn::Result<Self> {
        if let Ok(func) = syn::parse2::<ItemFn>(item.clone()) {
            // A receiver makes it an associated item. A receiverless associated function looks
            // like a free one here.
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

    /// A module reached by descending into one; grouped on its own.
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

    /// Rewrite every recursion in this scope. `thread_out`: re-export the module's functions
    /// beside it (only for the annotated module).
    pub(super) fn expand(self, opts: Opts, thread_out: bool) -> syn::Result<TokenStream> {
        Ok(self.expand_reporting(opts, thread_out)?.0)
    }

    /// Like [`Self::expand`], but an error if nothing was rewritten.
    pub(super) fn expand_annotated(self, opts: Opts) -> syn::Result<TokenStream> {
        let complaint = self.host.no_effect();
        let (tokens, transformed) = self.expand_reporting(opts, true)?;
        match complaint {
            Some(err) if !transformed => Err(err),
            _ => Ok(tokens),
        }
    }

    /// Like [`Self::expand`], also returning whether anything was rewritten (including in
    /// nested containers).
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
        // A marker on the container shadows the enclosing options.
        let opts = own_opts.unwrap_or(opts);
        let scanned = expand_roots(Roots {
            funcs,
            opts: opts.clone(),
            self_ty: host.self_ty(),
            host_name: host.host_name(),
            assoc: host.assoc(),
            trait_impl: host.trait_impl(),
        })?;
        host.rebuild(scanned, opts, thread_out)
    }
}

impl Host {
    /// Whether the roots are associated items: `self.g(..)` and `Self::g(..)` reach them, a
    /// bare `g(..)` does not.
    fn assoc(&self) -> bool {
        match self {
            Host::Fn { assoc, .. } => *assoc,
            Host::Mod(_) => false,
            Host::Impl(_) => true,
        }
    }

    /// Whether this is a trait impl.
    fn trait_impl(&self) -> bool {
        matches!(self, Host::Impl(block) if block.trait_.is_some())
    }

    /// The impl block's self type, for declaring a methods group's seed enum beside the block.
    /// `None` for a generic impl, whose group is left unlifted.
    fn self_ty(&self) -> Option<&syn::Type> {
        match self {
            Host::Impl(block) if block.generics.params.is_empty() => Some(&block.self_ty),
            _ => None,
        }
    }

    /// The name a call may reach into this scope by, besides `Self` / `self`: the impl's self
    /// type or the module's ident. `None` for a lone function.
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

    /// The error if nothing in this scope recurses. `None` for a function, which reports this
    /// itself in `rebuild`.
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

/// Uncalled copies of the roots flagged in `wants_check`, as written, so the compiler still
/// checks the original, stack-based program.
///
/// The copies call each other, not the rewritten functions. Names defined more than once in
/// the scope are not renamed, since resolving them would be a guess.
fn originals(
    as_written: &[ItemFn],
    defs: &[scope::Def],
    wants_check: &[bool],
) -> Vec<Option<ItemFn>> {
    // Roots come first in `defs`, in order.
    let original =
        |i: usize| names::original(&as_written[i].sig.ident, defs[i].opts.original_suffix());
    let renames: HashMap<String, Ident> = (0..as_written.len())
        .filter(|&i| wants_check[i])
        .filter(|&i| {
            defs.iter()
                .filter(|d| d.name == as_written[i].sig.ident)
                .count()
                == 1
        })
        .map(|i| (as_written[i].sig.ident.to_string(), original(i)))
        .collect();

    as_written
        .iter()
        .enumerate()
        .map(|(i, func)| {
            if !wants_check[i] {
                return None;
            }
            // Keeps the original's visibility, so it is reachable (and re-exported) alike.
            let mut copy = func.clone();
            copy.sig.ident = original(i);
            rename_calls(&mut copy, &renames);
            try_shim::desugar(&mut copy);
            // Only hard errors matter here; warnings are already reported against the original.
            copy.attrs.insert(0, syn::parse_quote!(#[allow(warnings)]));
            Some(copy)
        })
        .collect()
}

/// The options a cycle is rewritten under. Its members share one driver, so they must
/// agree; a mismatch is reported against the differing member.
fn agreed_opts(defs: &[scope::Def], cycle: &[usize]) -> syn::Result<Opts> {
    let (&host, rest) = cycle.split_first().expect("a cycle has a member");
    let opts = defs[host].opts.clone();
    for &member in rest {
        if !defs[member].opts.same_rewrite(&opts) {
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

/// Expand every module and impl block declared in this body, deepest first. Their
/// functions are not threaded out. Returns whether any were found.
fn expand_nested_containers(func: &mut ItemFn, opts: Opts) -> syn::Result<bool> {
    struct V {
        opts: Opts,
        found: bool,
        failed: Option<syn::Error>,
    }

    impl V {
        /// The tokens a nested container expands to, or `None` if `item` is not one.
        fn expanded(&mut self, item: &syn::Item) -> syn::Result<Option<TokenStream>> {
            match item {
                // A bodiless module never gets here: file modules in proc-macro input are unstable.
                syn::Item::Mod(inner) if inner.content.is_some() => {
                    let mut inner = inner.clone();
                    let own = Opts::take_from(&mut inner.attrs)?;
                    Scope::of_mod(inner)?
                        .expand(own.unwrap_or_else(|| self.opts.clone()), false)
                        .map(Some)
                }
                syn::Item::Impl(inner) => {
                    let mut inner = inner.clone();
                    let own = Opts::take_from(&mut inner.attrs)?;
                    Scope::of_impl(inner)?
                        .expand(own.unwrap_or_else(|| self.opts.clone()), false)
                        .map(Some)
                }
                _ => Ok(None),
            }
        }
    }

    impl VisitMut for V {
        fn visit_item_mut(&mut self, item: &mut syn::Item) {
            if let syn::Item::Fn(inner) = item {
                // A nested function's body may hold containers too.
                self.visit_block_mut(&mut inner.block);
                return;
            }
            match self.expanded(item) {
                // Keep the first error.
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

/// Reject a call that names a member through a path the rewriter cannot handle
/// (`T::g(..)`, `<Self>::g(..)`, `crate::m::g(..)`), but only if that call closes a cycle.
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
    // The graph with those calls added as edges.
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
    // Expand containers in bodies first; they are separate scopes.
    let mut changed = vec![false; roots.len()];
    for (i, root) in roots.iter_mut().enumerate() {
        changed[i] = expand_nested_containers(root, scope_opts.clone())?;
    }
    // Definitions in declaration order, markers removed, with the options in force at each.
    let defs = scope::collect(&mut roots, scope_opts)?;

    let (edges, blocked, ambiguous) = scope::edges(&roots, &defs, assoc, host_name.as_ref());
    // Two same-named functions in sibling blocks of one body: a definition is addressed by
    // its body, not its block, so the call is ambiguous.
    if let Some(name) = ambiguous.first() {
        return Err(syn::Error::new(
            name.span(),
            format!(
                "`{name}` names more than one function declared in this body, and `#[stack_safe]` \
                 cannot tell which: it addresses a definition by the body that declares it, not by \
                 the block, so two declared in sibling blocks are equally in scope here. Rust \
                 resolves this by block. Rename one of them, or move the one you mean out to the \
                 body itself"
            ),
        ));
    }
    let reaches = scope::closure(&edges);
    unresolvable_recursion(&defs, &edges, &reaches, &blocked, assoc)?;
    for (i, d) in defs.iter().enumerate() {
        // A marker that covers no recursion is an error.
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

    // Copies of the roots as written.
    let as_written: Vec<ItemFn> = roots.to_vec();
    let mut wants_check = vec![false; roots.len()];

    let mut out = Scanned {
        rewritten: vec![None; roots.len()],
        originals: vec![None; roots.len()],
        hoisted: Vec::new(),
    };

    for cycle in scope::cycles(&defs, &reaches) {
        // The outermost member, where this cycle is written. `cycle` is in `defs` order
        // (shallowest first), so it is the first.
        let host = &defs[cycle[0]];
        let depth = host.path.len();
        let inner: Vec<bool> = cycle.iter().map(|&j| defs[j].path.len() > depth).collect();

        // Take each member out of its body, deepest first so a nested member is removed before
        // its host is taken. Roots are copied, not taken.
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
        // Back to cycle order: the driver goes with the outermost member.
        members.reverse();

        // A recursive trait impl member cannot be rewritten: the rewrite needs a plain associated
        // function beside it, which a trait impl cannot hold. Cycles inside a member's body are fine.
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
        // Check every root this cycle touches.
        for &j in &cycle {
            wants_check[defs[j].owner] = true;
        }
        // `Self` is nameable in an arm only for a cycle among the impl block's own functions.
        let self_ty = if depth == 0 { self_ty } else { None };
        let (entries, hoisted) = expand_group(members, cycle_opts, self_ty, &inner, assoc)?;

        if depth == 0 {
            // Each member replaces its root; the driver goes with the first.
            for (&j, entry) in cycle.iter().zip(entries) {
                if defs[j].path.is_empty() {
                    out.rewritten[defs[j].owner] = Some(entry);
                }
            }
            if !hoisted.is_empty() {
                out.hoisted.push(hoisted);
            }
        } else {
            // No member is a root, so the whole cycle lies in one root's body; write it back where
            // the outermost member was.
            debug_assert!(
                cycle.iter().all(|&j| defs[j].owner == host.owner),
                "a cycle with no root among its members lies inside one root",
            );
            let root = &mut roots[host.owner];
            scope::put_back(root, &host.path, quote! { #hoisted #(#entries)* });
            changed[host.owner] = true;
        }
    }

    // A root in no cycle whose body held one is emitted as it now stands.
    for (i, root) in roots.iter().enumerate() {
        if out.rewritten[i].is_none() && changed[i] {
            out.rewritten[i] = Some(root.to_token_stream());
        }
    }
    out.originals = originals(&as_written, &defs, &wants_check);
    Ok(out)
}
