// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The CPS transform: each recursive call becomes a `call` transition plus a frame variant
//! holding the locals live across it; a loop whose body recurses becomes a new entry point.

use proc_macro2::{Ident, TokenStream};
use quote::{ToTokens, format_ident, quote};
use syn::spanned::Spanned;
use syn::visit_mut::VisitMut;
use syn::{Block, Expr, Pat, Stmt, parse_quote};

use super::analyze::{
    Lend, borrows_a_built_value, contains_rec, pat_bindings, stmt_contains_rec, tokens_mention,
};
use super::context::{CtxArg, classify_ctx_arg};
use super::driver;
use super::leaf::{leaf_expr, leaf_stmt};
use super::names::*;
use super::try_shim;
use super::walk::{Cont, Ctx, Env, Held, LoopCtx};

fn cps_block(ctx: &Ctx, env: &Env, block: &Block, k: Cont) -> syn::Result<TokenStream> {
    cps_stmts(ctx, env, &block.stmts, k)
}

pub(super) fn cps_stmts(ctx: &Ctx, env: &Env, stmts: &[Stmt], k: Cont) -> syn::Result<TokenStream> {
    let Some((first, rest)) = stmts.split_first() else {
        return k(quote! { () });
    };

    // A `#[cfg]` statement that recurses can't gate its pieces (the rest of the block lives
    // inside them), so lower the block both with and without it and pick one by the predicate.
    if stmt_contains_rec(ctx, first)
        && let Some(gate) = stmt_gate(first)?
    {
        let out = ctx.fresh();
        let with = ctx.under_gate(gate.clone(), || {
            let mut kept = first.clone();
            strip_cfg(&mut kept);
            let stmts: Vec<Stmt> = std::iter::once(kept).chain(rest.iter().cloned()).collect();
            cps_stmts(ctx, env, &stmts, k)
        })?;
        // `not`: these are the arms that exist when the predicate is false.
        let without = ctx.under_gate(quote! { not(#gate) }, || cps_stmts(ctx, env, rest, k))?;
        return Ok(quote! {
            {
                #[cfg(#gate)]
                let #out = #with;
                #[cfg(not(#gate))]
                let #out = #without;
                #out
            }
        });
    }

    if !stmt_contains_rec(ctx, first) {
        // The block's value. A block-like statement (`if c {..}`) also parses as
        // `Stmt::Expr(_, None)` even when statements follow it.
        if let Stmt::Expr(e, None) = first
            && rest.is_empty()
        {
            return k(leaf_expr(env, e)?);
        }
        let head = leaf_stmt(env, first)?;
        // Code after a diverging statement is skipped, not just for size: a loop lowered only in
        // dead code has nothing to infer its payload type from.
        if diverges(first) {
            return Ok(quote! { { #head } });
        }
        let env = match first {
            Stmt::Local(l) => env.bind(pat_bindings(&l.pat)),
            _ => env.clone(),
        };
        let tail = cps_stmts(ctx, &env, rest, k)?;
        return Ok(quote! { { #head #tail } });
    }

    match first {
        Stmt::Local(local) => {
            let init = local
                .init
                .as_ref()
                .expect("statement contains a recursive call, so it has an initializer");
            if let Some((_, diverge)) = &init.diverge {
                return Err(syn::Error::new(
                    diverge.span(),
                    "`#[stack_safe]` does not support a recursive call in a `let ... else` \
                     statement; bind the call first",
                ));
            }
            // `#[cfg]` was handled by `cps_stmts`; `cfg_attr` still has to be refused.
            reject_cfg_attr(&local.attrs, "a `let` whose initializer recurses")?;
            let pat = &local.pat;
            let attrs = &local.attrs;
            cps_expr(ctx, env, &init.expr, &|v| {
                let inner = env.bind(pat_bindings(&local.pat));
                let tail = cps_stmts(ctx, &inner, rest, k)?;
                Ok(quote! { { #(#attrs)* let #pat = #v; #tail } })
            })
        }
        // A `#[cfg]` here can't gate just this statement: the code after it is generated inside.
        Stmt::Expr(e, semi) => {
            reject_cfg_attr(&expr_attrs(e), "a statement that recurses")?;
            if semi.is_none() && rest.is_empty() {
                return cps_expr(ctx, env, e, k);
            }
            cps_expr(ctx, env, e, &|v| {
                let tail = cps_stmts(ctx, env, rest, k)?;
                Ok(quote! { { let _ = #v; #tail } })
            })
        }
        Stmt::Item(_) | Stmt::Macro(_) => unreachable!("checked by stmt_contains_rec / validate"),
    }
}

/// Hoist the value subexpressions out of a place, leaving only projections:
/// `&mut t.kids[idx()]` becomes `let __ss_vN = idx();` and `&mut t.kids[__ss_vN]`.
/// The root (the context) is left alone.
fn hoist_place(ctx: &Ctx, place: &Expr) -> (Vec<TokenStream>, Expr) {
    struct H<'a> {
        ctx: &'a Ctx,
        pre: Vec<TokenStream>,
    }

    impl H<'_> {
        fn take(&mut self, e: &mut Expr) {
            if matches!(e, Expr::Path(_) | Expr::Lit(_)) {
                return;
            }
            let tmp = self.ctx.fresh();
            self.pre.push(quote! { let #tmp = #e; });
            *e = parse_quote! { #tmp };
        }
    }

    impl VisitMut for H<'_> {
        fn visit_expr_mut(&mut self, e: &mut Expr) {
            match e {
                // Children first, to keep evaluation order.
                Expr::Index(i) => {
                    self.visit_expr_mut(&mut i.expr);
                    // A base that runs code (`node.select(..)[i]`) is hoisted to run before the
                    // index; a pure projection (`self.kids[i]`) stays part of the place.
                    if matches!(&*i.expr, Expr::MethodCall(_) | Expr::Call(_)) {
                        self.take(&mut i.expr);
                    }
                    self.take(&mut i.index);
                }
                Expr::MethodCall(m) => {
                    self.visit_expr_mut(&mut m.receiver);
                    for arg in m.args.iter_mut() {
                        self.take(arg);
                    }
                }
                Expr::Field(f) => self.visit_expr_mut(&mut f.base),
                Expr::Unary(u) => self.visit_expr_mut(&mut u.expr),
                Expr::Paren(p) => self.visit_expr_mut(&mut p.expr),
                Expr::Reference(r) => self.visit_expr_mut(&mut r.expr),
                // The root, or a shape `place_root` rejects.
                _ => {}
            }
        }
    }

    let mut h = H {
        ctx,
        pre: Vec::new(),
    };
    let mut place = place.clone();
    h.visit_expr_mut(&mut place);
    (h.pre, place)
}

/// Split a place into its inner values (in evaluation order) and the place with each value
/// replaced by its temporary.
///
/// Keeps a place a place across a cut: `xs[0].bump(f(n - 1))` must not copy `xs[0]` into a
/// temporary, or `bump(&mut self)` would mutate the copy. Only a path root is kept; any other
/// root, including a method call, is bound. Unlike [`hoist_place`], the result is used in a
/// later invocation of the body.
fn split_place(ctx: &Ctx, place: &Expr, later: &[&Expr]) -> (Vec<(Ident, Expr)>, Expr) {
    struct S<'a> {
        ctx: &'a Ctx,
        /// What the source evaluates after the place, which may write a path read from it.
        later: &'a [&'a Expr],
        values: Vec<(Ident, Expr)>,
    }

    impl S<'_> {
        fn bind(&mut self, e: &mut Expr) {
            let tmp = self.ctx.fresh();
            self.values.push((tmp.clone(), e.clone()));
            *e = parse_quote! { #tmp };
        }

        /// Bind an inner value, unless it is a plain read that nothing in `later` can change.
        fn take(&mut self, e: &mut Expr) {
            let stable = match &*e {
                Expr::Lit(_) => true,
                Expr::Path(p) => p
                    .path
                    .get_ident()
                    .is_some_and(|id| !self.later.iter().any(|l| mentions_ident(l, id))),
                _ => false,
            };
            if !stable {
                self.bind(e);
            }
        }

        /// Walk the projections down to the root, binding inner values.
        fn walk(&mut self, e: &mut Expr) {
            match e {
                // Children first, to keep evaluation order.
                Expr::Index(i) => {
                    self.walk(&mut i.expr);
                    self.take(&mut i.index);
                }
                Expr::Field(f) => self.walk(&mut f.base),
                Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => self.walk(&mut u.expr),
                Expr::Paren(p) => self.walk(&mut p.expr),
                Expr::Group(g) => self.walk(&mut g.expr),
                // A path root stays; any other root is evaluated here.
                Expr::Path(p) if p.qself.is_none() => {}
                root => self.bind(root),
            }
        }
    }

    let mut s = S {
        ctx,
        later,
        values: Vec::new(),
    };
    let mut place = place.clone();
    s.walk(&mut place);
    (s.values, place)
}

/// Does `e` mention `id`?
fn mentions_ident(e: &Expr, id: &Ident) -> bool {
    tokens_mention(&e.to_token_stream(), id)
}

/// The outer attributes of any expression (read back from its leading tokens).
fn expr_attrs(e: &Expr) -> Vec<syn::Attribute> {
    let parser = |input: syn::parse::ParseStream| {
        let attrs = input.call(syn::Attribute::parse_outer)?;
        input.parse::<TokenStream>()?;
        Ok(attrs)
    };
    syn::parse::Parser::parse2(parser, e.to_token_stream()).unwrap_or_default()
}

/// The `#[cfg]` predicate in `attrs`, if any; several are combined with `all(..)`.
fn cfg_gate(attrs: &[syn::Attribute]) -> syn::Result<Option<TokenStream>> {
    reject_cfg_attr(attrs, "code that recurses")?;
    let preds: Vec<TokenStream> = attrs
        .iter()
        .filter(|a| a.path().is_ident("cfg"))
        .map(|a| a.parse_args::<TokenStream>())
        .collect::<syn::Result<_>>()?;
    Ok(match preds.len() {
        0 => None,
        1 => preds.into_iter().next(),
        _ => Some(quote! { all(#(#preds),*) }),
    })
}

/// A statement's `#[cfg]` predicate.
fn stmt_gate(stmt: &Stmt) -> syn::Result<Option<TokenStream>> {
    match stmt {
        Stmt::Local(local) => cfg_gate(&local.attrs),
        Stmt::Expr(e, _) => cfg_gate(&expr_attrs(e)),
        Stmt::Item(_) | Stmt::Macro(_) => Ok(None),
    }
}

/// Remove the `#[cfg]`s from a statement.
fn strip_cfg(stmt: &mut Stmt) {
    fn drop_cfgs(attrs: &mut Vec<syn::Attribute>) {
        attrs.retain(|a| !a.path().is_ident("cfg"));
    }
    match stmt {
        Stmt::Local(local) => drop_cfgs(&mut local.attrs),
        Stmt::Expr(e, _) => drop_expr_cfgs(e),
        Stmt::Item(_) | Stmt::Macro(_) => {}
    }
}

/// Remove the `#[cfg]`s from an expression.
fn drop_expr_cfgs(e: &mut Expr) {
    macro_rules! strip {
        ($($variant:ident),*) => {
            match e {
                $(Expr::$variant(inner) => inner.attrs.retain(|a| !a.path().is_ident("cfg")),)*
                _ => {}
            }
        };
    }
    strip!(
        Array, Assign, Async, Await, Binary, Block, Break, Call, Cast, Closure, Const, Continue,
        Field, ForLoop, Group, If, Index, Infer, Let, Lit, Loop, Macro, Match, MethodCall, Paren,
        Path, Range, Reference, Repeat, Return, Struct, Try, TryBlock, Tuple, Unary, Unsafe, While,
        Yield
    );
}

/// Reject a `#[cfg]` outside statements, match arms and struct fields: elsewhere the pieces
/// of a cut move to other arms, so dropping it would run disabled code.
fn reject_cfg(attrs: &[syn::Attribute], what: &str) -> syn::Result<()> {
    reject_cfg_attr(attrs, what)?;
    for attr in attrs {
        if attr.path().is_ident("cfg") {
            return Err(syn::Error::new_spanned(
                attr,
                format!(
                    "`#[stack_safe]` cannot honour a `#[cfg]` on {what}: the recursive call in \
                     it is cut into a state machine, so the gated code does not stay in one \
                     piece and there is nothing left to gate — what the `#[cfg]` disables \
                     would compile and run. Put the `#[cfg]` on the whole statement, on a match \
                     arm, or on an item"
                ),
            ));
        }
    }
    Ok(())
}

/// Reject a `#[cfg_attr]` on something a recursive call is cut out of: its expansion may only
/// make sense in the original position.
fn reject_cfg_attr(attrs: &[syn::Attribute], what: &str) -> syn::Result<()> {
    for attr in attrs {
        if attr.path().is_ident("cfg_attr") {
            return Err(syn::Error::new_spanned(
                attr,
                format!(
                    "`#[stack_safe]` cannot honour a `#[cfg_attr]` on {what}: the recursive call \
                     in it is cut into a state machine, and what this expands to may only mean \
                     something where it was written. Use a plain `#[cfg]`, or put this on an item"
                ),
            ));
        }
    }
    Ok(())
}

/// CPS an expression used as a place: evaluate its inner values, then call `k` with the place.
/// `later` is what the source evaluates after it (e.g. method arguments).
fn cps_place(
    ctx: &Ctx,
    env: &Env,
    place: &Expr,
    later: &[&Expr],
    k: &dyn Fn(&Env, &Expr) -> syn::Result<TokenStream>,
) -> syn::Result<TokenStream> {
    fn bind_values(
        ctx: &Ctx,
        env: &Env,
        values: &[(Ident, Expr)],
        place: &Expr,
        k: &dyn Fn(&Env, &Expr) -> syn::Result<TokenStream>,
    ) -> syn::Result<TokenStream> {
        let Some(((tmp, value), rest)) = values.split_first() else {
            return k(env, place);
        };
        // The temporary must be visible to the continuation, which may thread it.
        let inner = env.bind([tmp.clone()]);
        if !contains_rec(ctx, value) {
            let head = leaf_expr(env, value)?;
            let tail = bind_values(ctx, &inner, rest, place, k)?;
            return Ok(quote! { { let #tmp = #head; #tail } });
        }
        cps_expr(ctx, env, value, &|v| {
            let tail = bind_values(ctx, &inner, rest, place, k)?;
            Ok(quote! { { let #tmp = #v; #tail } })
        })
    }

    let (values, place) = split_place(ctx, place, later);
    bind_values(ctx, env, &values, &place, k)
}

/// Does this statement always leave the block (a bare `return`, `break` or `continue`)?
fn diverges(stmt: &Stmt) -> bool {
    let Stmt::Expr(e, _) = stmt else { return false };
    matches!(e, Expr::Return(_) | Expr::Break(_) | Expr::Continue(_))
}

fn cps_expr(ctx: &Ctx, env: &Env, e: &Expr, k: Cont) -> syn::Result<TokenStream> {
    // No recursive call: splice as is.
    if !contains_rec(ctx, e) {
        return k(leaf_expr(env, e)?);
    }
    // The expression is rebuilt, so its attributes are lost; only `#[cfg]` matters (`reject_cfg`).
    reject_cfg(&expr_attrs(e), "an expression that recurses")?;

    let entry = entry_ty();
    let frame = frame_ty();

    // ---- the recursive call itself --------------------------------------
    if let Some((callee, call)) = ctx.rec_call(e) {
        let callee_variant = entry_variant(callee);
        // Context arguments skip the payload: either the child shares the parent's slot, or it
        // gets a derived place that is swapped in and restored by the continuation.
        let mut payload: Vec<Expr> = Vec::new();
        let mut swaps: Vec<(usize, Expr)> = Vec::new();
        let ctxp = ctx_param();
        // A pinned position lends a value built here: it moves into that position's store and
        // a pointer travels in the payload. Other arguments there become pointers too.
        let mut pinned_slots: Vec<syn::Index> = Vec::new();
        // Locals parked to lend a place inside them: (store, name, index among this call's pushes).
        let mut parked: Vec<(Held, Ident, usize)> = Vec::new();
        let mut pushes = 0usize;
        for (i, arg) in call.args.iter().enumerate() {
            match ctx.member(callee).context_at.get(&i) {
                None => {
                    let j = payload.len();
                    if ctx.member(callee).pinned[j].get() {
                        let held = ctx.held_pin(callee, j);
                        let slot = held.slot();
                        payload.push(match borrows_a_built_value(ctx, ctx.current.get(), arg) {
                            Some(Lend::Whole(built)) => {
                                if !pinned_slots.contains(&slot) {
                                    pinned_slots.push(slot.clone());
                                }
                                pushes += 1;
                                push_into(&held, built, &|_| None)
                            }
                            // Park the local and point inside it; the resume arm takes it back.
                            Some(Lend::Place { root, place }) => {
                                let held = ctx.held_root(&root);
                                let slot = held.slot();
                                if !pinned_slots.contains(&slot) {
                                    pinned_slots.push(slot.clone());
                                }
                                parked.push((held.clone(), root.clone(), pushes));
                                pushes += 1;
                                let root_name = root.clone();
                                push_into(&held, &root_expr(&root), &|owned| {
                                    Some(project_from(place, &root_name, owned))
                                })
                            }
                            None => parse_quote! { ::core::ptr::from_ref(#arg) },
                        });
                    } else {
                        payload.push(arg.clone());
                    }
                }
                Some(&slot) => match classify_ctx_arg(arg, &ctx.context) {
                    Some(CtxArg::Same) | None => {}
                    Some(CtxArg::Derived(place)) => swaps.push((slot, place)),
                },
            }
        }
        let payload: Vec<&Expr> = payload.iter().collect();
        // One mark per store, taken before any argument runs. Fresh names, since an argument may
        // itself contain a call.
        let marks: Vec<(syn::Index, Ident)> = pinned_slots
            .iter()
            .map(|slot| (slot.clone(), ctx.fresh()))
            .collect();
        let out = cps_seq(ctx, env, &payload, Vec::new(), &|vals| {
            let v = ctx.fresh();
            let mut saved: Vec<Ident> = swaps.iter().map(|(slot, _)| saved_slot(*slot)).collect();
            // Forced into the payload even if the continuation doesn't mention it.
            for (_, mark) in &marks {
                saved.push(mark.clone());
            }

            // Reserve the resume point first so nested calls get later indices. Parked locals
            // are not carried: the resume arm takes them back from the store.
            let mut scope = ctx.scope_with_results(&env.scope);
            scope.retain(|name| !parked.iter().any(|(_, root, _)| root == name));
            // Nothing to run before this point's code, so a leading `?` can be shared.
            let bare = parked.is_empty() && marks.is_empty() && swaps.is_empty();
            let derived = env
                .derived
                .iter()
                .filter(|d| !parked.iter().any(|(_, root, _)| root == &d.name))
                .cloned()
                .collect();
            let r = ctx.reserve_resume(scope, derived, saved.clone(), v.clone(), bare);
            let frame_var = frame_variant(r);
            let marker = frame_marker(r);

            let body = ctx.with_result(v.clone(), || k(quote! { #v }))?;
            let prologue = ctx.ctx_prologue();

            // Unwrap the union variant when members' return types differ (not needed when the
            // `?` check was lifted).
            let unwrap = if ctx.is_checked(r) {
                ctx.note_unwrapped(callee, &v);
                TokenStream::new()
            } else {
                ctx.unwrap_result(callee, &v)
            };

            // Restore swapped pointers before the prologue re-derives context bindings.
            let restores = swaps.iter().map(|(slot, _)| {
                let (saved, idx) = (saved_slot(*slot), syn::Index::from(*slot));
                quote! { #ctxp.#idx = #saved; }
            });
            // Take parked locals back, last first, so each `take_at` drops only later pushes.
            let take_backs = parked.iter().rev().map(|(held, root, at)| {
                let mark = marks
                    .iter()
                    .find(|(slot, _)| *slot == held.slot())
                    .map(|(_, mark)| mark.clone())
                    .expect("a call that parks takes a mark for that store");
                take_back(held, root, &mark, *at)
            });
            let take_backs = quote! { #(#take_backs)* };
            // Drop what this call lent the callee.
            let unpin = marks
                .iter()
                .map(|(slot, mark)| quote! { #ctxp.#slot.truncate(#mark); });
            let unpin = quote! { #(#unpin)* };
            // Tail call: if the continuation is the identity (`driver::done(v)`) and nothing runs
            // before it (no take-backs, store release, swaps, or non-rebinding prologue), enter the
            // callee without pushing a frame. Excluded: groups that re-wrap into a union, and
            // points whose `?` was lifted.
            let is_tail = take_backs.is_empty()
                && unpin.is_empty()
                && swaps.is_empty()
                && ctx.ctx_prologue_only_rebinds()
                && !ctx.is_checked(r)
                && body.to_string() == driver::done(quote! { #v }).to_string();
            // Drop the unused resume point so no frame variant is generated for it.
            let is_tail = is_tail && ctx.drop_last_resume(r);
            if !is_tail {
                ctx.set_resume_code(
                    r,
                    quote! {
                        #take_backs
                        #unpin
                        #(#restores)*
                        #prologue
                        #unwrap
                        #body
                    },
                );
            }

            // Without a swap, arguments stay inline.
            let call = if swaps.is_empty() {
                // One `let` per argument, in source order (see `driver::call`). Pinned positions
                // hold store pointers, so they are unannotated.
                let tmps: Vec<Ident> = vals.iter().map(|_| ctx.fresh()).collect();
                let args = tmps.iter().zip(vals).enumerate().map(|(j, (tmp, val))| {
                    let ann = if ctx.member(callee).pinned[j].get() {
                        TokenStream::new()
                    } else {
                        ctx.member(callee)
                            .param_types
                            .get(j)
                            .cloned()
                            .unwrap_or_default()
                    };
                    quote! { let #tmp #ann = #val; }
                });
                let args = quote! { #(#args)* };
                let enter = quote! { #entry::#callee_variant((#(#tmps,)*)) };
                match is_tail {
                    true => driver::enter(args, enter),
                    false => driver::call(args, enter, quote! { #frame::#frame_var(#marker) }),
                }
            } else {
                // With a swap: bind every argument in source order, park each parent pointer (it's
                // `Copy`), and take the derived pointer from the hoisted place.
                let swap_for = |slot: usize, place: &Expr| {
                    let (saved, idx) = (saved_slot(slot), syn::Index::from(slot));
                    let derived = if ctx.context[slot].mutable {
                        quote! { ::core::ptr::from_mut(#place) }
                    } else {
                        quote! { ::core::ptr::from_ref(#place) }
                    };
                    quote! {
                        let #saved = #ctxp.#idx;
                        #ctxp.#idx = #derived;
                    }
                };

                // Take the derived pointer where the source does; borrowck keeps later arguments
                // from touching the parent. Escaping arguments run the restores first
                // (`Env::restores`). Only a recursing argument forces the swap to the end.
                let defer_after =
                    |i: usize| call.args.iter().skip(i + 1).any(|a| contains_rec(ctx, a));

                let mut pre: Vec<TokenStream> = Vec::new();
                let mut deferred: Vec<TokenStream> = Vec::new();
                let mut pending = TokenStream::new();
                let mut held: Vec<TokenStream> = Vec::new();
                let mut payload_seen = 0usize;
                for (i, _) in call.args.iter().enumerate() {
                    match ctx.member(callee).context_at.get(&i) {
                        None => {
                            let ann = if ctx.member(callee).pinned[payload_seen].get() {
                                // A pointer into the pinned store.
                                TokenStream::new()
                            } else {
                                ctx.member(callee)
                                    .param_types
                                    .get(payload_seen)
                                    .cloned()
                                    .unwrap_or_default()
                            };
                            // Re-lowered from `payload` (not reused from `cps_seq`) so escapes
                            // get the restores of earlier swaps and built values go to the store.
                            let value = if pending.is_empty() {
                                vals[payload_seen].clone()
                            } else {
                                leaf_expr(
                                    &env.with_restores(pending.clone()),
                                    payload[payload_seen],
                                )?
                            };
                            let tmp = ctx.fresh();
                            pre.push(quote! { let #tmp #ann = #value; });
                            held.push(quote! { #tmp });
                            payload_seen += 1;
                        }
                        Some(&slot) => {
                            if let Some((_, place)) = swaps.iter().find(|(s, _)| *s == slot) {
                                let (hoists, place) = hoist_place(ctx, place);
                                pre.extend(hoists);
                                if defer_after(i) {
                                    deferred.push(swap_for(slot, &place));
                                } else {
                                    pre.push(swap_for(slot, &place));
                                    let (saved, idx) = (saved_slot(slot), syn::Index::from(slot));
                                    pending.extend(quote! { #ctxp.#idx = #saved; });
                                }
                            }
                        }
                    }
                }

                driver::call(
                    quote! { #(#pre)* #(#deferred)* },
                    quote! { #entry::#callee_variant((#(#held,)*)) },
                    quote! { #frame::#frame_var(#marker) },
                )
            };
            Ok(quote! { { #call } })
        });
        let out = out?;
        return Ok(if marks.is_empty() {
            out
        } else {
            let taken = marks
                .iter()
                .map(|(slot, mark)| quote! { let #mark = #ctxp.#slot.mark(); });
            quote! { { #(#taken)* #out } }
        });
    }

    match e {
        // ---- branching: the continuation is duplicated into each arm ----
        Expr::If(if_expr) => {
            if let Expr::Let(l) = &*if_expr.cond {
                if contains_rec(ctx, &l.expr) {
                    return Err(syn::Error::new(
                        l.span(),
                        "`#[stack_safe]` does not support a recursive call in an `if let` \
                         scrutinee; bind it to a `let` first",
                    ));
                }
                // `if let` binds in the then-branch only.
                let cond = leaf_expr(env, &if_expr.cond)?;
                let inner = env.bind(pat_bindings(&l.pat));
                let then_ts = cps_block(ctx, &inner, &if_expr.then_branch, k)?;
                let else_ts = match &if_expr.else_branch {
                    Some((_, alt)) => cps_expr(ctx, env, alt, k)?,
                    None => k(quote! { () })?,
                };
                return Ok(quote! { if #cond { #then_ts } else { #else_ts } });
            }
            let cond = &if_expr.cond;
            let then = &if_expr.then_branch;
            cps_expr(ctx, env, cond, &|c| {
                let then_ts = cps_block(ctx, env, then, k)?;
                let else_ts = match &if_expr.else_branch {
                    Some((_, alt)) => cps_expr(ctx, env, alt, k)?,
                    None => k(quote! { () })?,
                };
                Ok(quote! { if #c { #then_ts } else { #else_ts } })
            })
        }

        Expr::Match(m) => {
            let scrutinee = &m.expr;
            cps_expr(ctx, env, scrutinee, &|s| {
                let mut arms = Vec::new();
                for arm in &m.arms {
                    // syn represents a match guard as `Pat::Guard`.
                    let (pat, guard) = match &arm.pat {
                        Pat::Guard(g) => (&*g.pat, Some(&*g.guard)),
                        other => (other, None),
                    };
                    if let Some(guard) = guard
                        && contains_rec(ctx, guard)
                    {
                        return Err(syn::Error::new(
                            guard.span(),
                            "`#[stack_safe]` does not support a recursive call in a match \
                                 guard",
                        ));
                    }
                    let inner = env.bind(pat_bindings(pat));
                    let guard = match guard {
                        Some(g) => {
                            let g = leaf_expr(&inner, g)?;
                            quote! { if #g }
                        }
                        None => quote! {},
                    };
                    // A `#[cfg]` on the arm also gates the driver arms for calls inside it.
                    let body = match cfg_gate(&arm.attrs)? {
                        None => cps_expr(ctx, &inner, &arm.body, k)?,
                        Some(gate) => {
                            ctx.under_gate(gate, || cps_expr(ctx, &inner, &arm.body, k))?
                        }
                    };
                    // Keep the arm's attributes; dropping a `#[cfg]` would duplicate the arm.
                    let attrs = &arm.attrs;
                    arms.push(quote! { #(#attrs)* #pat #guard => #body, });
                }
                Ok(quote! { match #s { #(#arms)* } })
            })
        }

        Expr::Block(b) => cps_block(ctx, env, &b.block, k),
        Expr::Paren(p) => cps_expr(ctx, env, &p.expr, k),
        Expr::Group(g) => cps_expr(ctx, env, &g.expr, k),

        Expr::Return(r) => {
            let inner: Expr = match &r.expr {
                Some(v) => (**v).clone(),
                None => parse_quote! { () },
            };
            cps_expr(ctx, env, &inner, &|v| {
                let v = env.wrapped(v);
                Ok(driver::escape(driver::done(v)))
            })
        }

        Expr::Try(t) => cps_expr(ctx, env, &t.expr, &|v| {
            // `f(a)?`: the check can be lifted out of the resume point and shared above the frame
            // dispatch, unless a swap or store release is pending (the error path must undo it).
            if ctx.hoist.get()
                && env.restores.is_empty()
                && env.teardown.is_empty()
                && ctx.mark_checked(&v)
            {
                return k(v);
            }
            let ok = ctx.fresh();
            // `ok` is ours; record it as live so later calls (`f()? + g()?`) carry it.
            let body = ctx.with_result(ok.clone(), || k(quote! { #ok }))?;
            let branch = try_shim::branch(v.clone());
            let exit = env.wrapped(try_shim::from_residual(quote! { __ss_res }));
            let exit = driver::escape(driver::done(exit));
            Ok(quote! {
                match #branch {
                    ::core::result::Result::Ok(#ok) => #body,
                    ::core::result::Result::Err(__ss_res) => { #exit }
                }
            })
        }),

        // ---- loops ------------------------------------------------------
        Expr::ForLoop(_) | Expr::While(_) | Expr::Loop(_) => lower_loop(ctx, env, e, k),

        // `break` / `continue` in recursing code (not leaf-rewritten).
        Expr::Continue(c) => {
            if c.label.is_some() {
                return Err(syn::Error::new(
                    c.span(),
                    "`#[stack_safe]` does not support labelled `continue` in a loop that contains \
                     a recursive call",
                ));
            }
            match env.lp {
                Some(lp) => {
                    let v = entry_variant(lp.variant);
                    let marker = state_marker(lp.idx);
                    let advance = &lp.advance;
                    let enter = driver::tail(quote! { #entry::#v(#marker) });
                    Ok(driver::escape(quote! { { #advance #enter } }))
                }
                None => Err(syn::Error::new(c.span(), "`continue` outside of a loop")),
            }
        }
        Expr::Break(b) => {
            if b.label.is_some() {
                return Err(syn::Error::new(
                    b.span(),
                    "`#[stack_safe]` does not support labelled `break` in a loop that contains a \
                     recursive call",
                ));
            }
            let Some(lp) = env.lp else {
                return Err(syn::Error::new(b.span(), "`break` outside of a loop"));
            };
            match &b.expr {
                Some(v) => cps_expr(ctx, env, v, lp.brk),
                None => (lp.brk)(quote! { () }),
            }
        }

        // ---- short-circuit operators: the RHS is conditional ------------
        Expr::Binary(b) if matches!(b.op, syn::BinOp::And(_) | syn::BinOp::Or(_)) => {
            let is_and = matches!(b.op, syn::BinOp::And(_));
            cps_expr(ctx, env, &b.left, &|l| {
                let rhs = cps_expr(ctx, env, &b.right, k)?;
                let shortcut = k(if is_and {
                    quote! { false }
                } else {
                    quote! { true }
                })?;
                Ok(if is_and {
                    quote! { if #l { #rhs } else { #shortcut } }
                } else {
                    quote! { if #l { #shortcut } else { #rhs } }
                })
            })
        }

        // Compound assignment: the left operand is a place, so it is not bound to a temporary.
        Expr::Binary(b) if is_assign_op(&b.op) => {
            if contains_rec(ctx, &b.left) {
                return Err(syn::Error::new(
                    b.left.span(),
                    "`#[stack_safe]` does not support a recursive call on the left-hand side of \
                     a compound assignment",
                ));
            }
            let lhs = leaf_expr(env, &b.left)?;
            let op = &b.op;
            cps_expr(ctx, env, &b.right, &|v| {
                let tail = k(quote! { () })?;
                Ok(quote! { { #lhs #op #v; #tail } })
            })
        }

        // ---- strict positions: evaluate left to right ------------------
        Expr::Binary(b) => {
            let op = &b.op;
            cps_seq(ctx, env, &[&b.left, &b.right], Vec::new(), &|v| {
                let (l, r) = (&v[0], &v[1]);
                k(quote! { (#l #op #r) })
            })
        }
        Expr::Unary(u) => {
            let op = &u.op;
            cps_expr(ctx, env, &u.expr, &|v| k(quote! { (#op #v) }))
        }
        Expr::Cast(c) => {
            let ty = &c.ty;
            cps_expr(ctx, env, &c.expr, &|v| k(quote! { (#v as #ty) }))
        }
        Expr::Reference(r) => {
            let m = &r.mutability;
            cps_expr(ctx, env, &r.expr, &|v| k(quote! { (& #m #v) }))
        }
        Expr::Field(f) => {
            let member = &f.member;
            cps_expr(ctx, env, &f.base, &|v| k(quote! { (#v).#member }))
        }
        // An index is a place (`&mut xs[i]`, `xs[i].bump(..)`); only its inner values run here.
        Expr::Index(_) => cps_place(ctx, env, e, &[], &|_, place| k(quote! { (#place) })),
        Expr::Tuple(t) => {
            let elems: Vec<&Expr> = t.elems.iter().collect();
            cps_seq(ctx, env, &elems, Vec::new(), &|v| k(quote! { (#(#v,)*) }))
        }
        Expr::Array(a) => {
            let elems: Vec<&Expr> = a.elems.iter().collect();
            cps_seq(ctx, env, &elems, Vec::new(), &|v| k(quote! { [#(#v),*] }))
        }
        Expr::Call(call) => {
            let args: Vec<&Expr> = call.args.iter().collect();
            // The callee runs before the arguments, so a computed one is sequenced first
            // (`cps_seq` binds it). A path callee stays: binding a generic fn item hurts inference.
            if matches!(&*call.func, Expr::Path(_)) {
                let func = &call.func;
                return cps_seq(ctx, env, &args, Vec::new(), &|v| {
                    k(quote! { #func(#(#v),*) })
                });
            }
            let mut parts: Vec<&Expr> = Vec::with_capacity(args.len() + 1);
            parts.push(&call.func);
            parts.extend(args);
            cps_seq(ctx, env, &parts, Vec::new(), &|v| {
                let (func, args) = v.split_first().expect("the callee was pushed first");
                k(quote! { (#func)(#(#args),*) })
            })
        }
        Expr::MethodCall(mc) => {
            let method = &mc.method;
            let turbofish = &mc.turbofish;
            // The receiver is a place (the method may take `&mut self`); see [`split_place`].
            let args: Vec<&Expr> = mc.args.iter().collect();
            cps_place(ctx, env, &mc.receiver, &args, &|env, recv| {
                cps_seq(ctx, env, &args, Vec::new(), &|v| {
                    k(quote! { (#recv).#method #turbofish (#(#v),*) })
                })
            })
        }
        Expr::Struct(s) => {
            let path = &s.path;
            // A gated field can't keep its attribute, so lower the expression with and without it.
            if let Some((at, gate)) = s
                .fields
                .iter()
                .enumerate()
                .find_map(|(i, f)| cfg_gate(&f.attrs).map(|g| g.map(|g| (i, g))).transpose())
                .transpose()?
            {
                let out = ctx.fresh();
                let with = ctx.under_gate(gate.clone(), || {
                    let mut kept = s.clone();
                    kept.fields[at].attrs.retain(|a| !a.path().is_ident("cfg"));
                    cps_expr(ctx, env, &Expr::Struct(kept), k)
                })?;
                let mut dropped = s.clone();
                let mut fields = dropped.fields.into_iter().collect::<Vec<_>>();
                fields.remove(at);
                dropped.fields = fields.into_iter().collect();
                let without = ctx.under_gate(quote! { not(#gate) }, || {
                    cps_expr(ctx, env, &Expr::Struct(dropped), k)
                })?;
                return Ok(quote! {
                    {
                        #[cfg(#gate)]
                        let #out = #with;
                        #[cfg(not(#gate))]
                        let #out = #without;
                        #out
                    }
                });
            }
            let names: Vec<&syn::Member> = s.fields.iter().map(|f| &f.member).collect();
            let vals: Vec<&Expr> = s.fields.iter().map(|f| &f.expr).collect();
            let rest = match &s.rest {
                Some(r) => {
                    if contains_rec(ctx, r) {
                        return Err(syn::Error::new(
                            r.span(),
                            "`#[stack_safe]` does not support a recursive call in struct update \
                             syntax (`..base`)",
                        ));
                    }
                    let r = leaf_expr(env, r)?;
                    // No comma here: each field already emits one.
                    quote! { .. #r }
                }
                None => quote! {},
            };
            cps_seq(ctx, env, &vals, Vec::new(), &|v| {
                k(quote! { #path { #(#names: #v,)* #rest } })
            })
        }
        Expr::Assign(a) => {
            if contains_rec(ctx, &a.left) {
                return Err(syn::Error::new(
                    a.left.span(),
                    "`#[stack_safe]` does not support a recursive call on the left-hand side of \
                     an assignment",
                ));
            }
            let lhs = leaf_expr(env, &a.left)?;
            cps_expr(ctx, env, &a.right, &|v| {
                let tail = k(quote! { () })?;
                Ok(quote! { { #lhs = #v; #tail } })
            })
        }

        Expr::Closure(_) => Err(syn::Error::new(
            e.span(),
            "`#[stack_safe]` cannot rewrite a recursive call inside a closure: the closure is \
             invoked by code the macro cannot see. Hoist the call out of the closure, or use a \
             `for` loop.",
        )),
        Expr::Await(_) => Err(syn::Error::new(
            e.span(),
            "`#[stack_safe]` does not support `.await`",
        )),
        Expr::Async(_) | Expr::Const(_) => Err(syn::Error::new(
            e.span(),
            "`#[stack_safe]` cannot rewrite a recursive call inside this block",
        )),
        other => Err(syn::Error::new(
            other.span(),
            "`#[stack_safe]` does not support a recursive call in this position; bind it to a \
             `let` first",
        )),
    }
}

/// Lower a loop whose body recurses into a new entry point. Each iteration is a `tail`
/// re-entry (no frame pushed); the iterator and live locals travel in the entry payload.
fn lower_loop(ctx: &Ctx, env: &Env, e: &Expr, k: Cont) -> syn::Result<TokenStream> {
    let entry = entry_ty();
    let ctxp = ctx_param();

    let iter_ident = match e {
        Expr::ForLoop(_) => Some(format_ident!("__ss_it{}", ctx.loops.borrow().len())),
        _ => None,
    };

    // A `for` over a borrow moves the collection into the store, since the iterator is carried
    // in the payload and can't borrow a local.
    let store = match e {
        Expr::ForLoop(f) => borrowed_owner(&f.expr),
        _ => None,
    };
    let store = match store {
        None => None,
        Some(owner) => {
            if !ctx.opts.data_in_frame {
                return Err(syn::Error::new(
                    owner.span(),
                    format!(
                        "`#[stack_safe]` cannot park an iterator that borrows `{owner}`, because \
                         the frame holding it owns `{owner}` too. Enable \
                         `#[stack_safe(data_in_frame)]` to move `{owner}` into the driver's store \
                         for the loop, or iterate it by value (`for x in {owner}`) or by index",
                    ),
                ));
            }
            let Some(elem) = ctx.current_param_type(&owner) else {
                return Err(syn::Error::new(
                    owner.span(),
                    format!(
                        "`#[stack_safe]` cannot name the type of `{owner}`, so it cannot build the \
                         store this loop needs; `{owner}` has to be a parameter of the function. \
                         Iterate it by value (`for x in {owner}`) or by index instead",
                    ),
                ));
            };
            let held = ctx.held_loop(ctx.loops.borrow().len(), elem.clone());
            Some((owner, held, ctx.fresh(), elem))
        }
    };
    // `for x in a..b`: advance at the end of the iteration, so the range's `start` is this
    // iteration's value and frames needn't also carry `x` (see `walk::Derived`).
    let peeked = match (e, &store, &iter_ident) {
        (Expr::ForLoop(f), None, Some(it)) if is_bounded_range(&f.expr) => {
            plain_binding(&f.pat).map(|name| (name, it.clone()))
        }
        _ => None,
    };
    let advance = match &peeked {
        Some((_, it)) => quote! { let _ = ::core::iter::Iterator::next(&mut #it); },
        None => TokenStream::new(),
    };
    let store_forced: Vec<Ident> = store.iter().map(|(_, _, mark, _)| mark.clone()).collect();
    // Not `Env::restores`: those also run on `continue`, which still needs the collection.
    let release = match &store {
        Some((_, held, mark, _)) => {
            let slot = held.slot();
            quote! { #ctxp.#slot.truncate(#mark); }
        }
        None => TokenStream::new(),
    };

    let idx = ctx.reserve_loop(
        ctx.scope_with_results(&env.scope),
        env.derived.clone(),
        iter_ident.clone(),
        store_forced,
    );
    let variant = entry_variant(ctx.loop_base() + idx);
    let marker = state_marker(idx);

    // Leaving the loop releases the store (`?` and `return` use `Env::teardown`). Bind the
    // value first, since it may live in the store.
    let released_k = |v: TokenStream| -> syn::Result<TokenStream> {
        if release.is_empty() {
            return k(v);
        }
        let after = k(quote! { __ss_left_loop })?;
        let release = &release;
        Ok(quote! { { let __ss_left_loop = #v; #release #after } })
    };
    let k: Cont = &released_k;

    // `continue` re-enters this entry point; `break` runs the code after the loop.
    let lp = LoopCtx {
        idx,
        variant: ctx.loop_base() + idx,
        brk: k,
        advance: advance.clone(),
    };
    // The iterator and store marks are entry-point bindings that resume points must thread.
    let store_bindings: Vec<Ident> = store.iter().map(|(_, _, mark, _)| mark.clone()).collect();
    let lenv = env
        .with_teardown(release.clone())
        .in_loop(&lp)
        .bind(iter_ident.clone())
        .bind(store_bindings);
    let enter = driver::tail(quote! { #entry::#variant(#marker) });
    let again = quote! { { #advance #enter } };
    // Evaluate and discard the body's value: it may be a whole expression with side effects.
    let next =
        |v: TokenStream| -> syn::Result<TokenStream> { Ok(quote! { { let _ = #v; #again } }) };

    let arm = match e {
        Expr::ForLoop(f) => {
            let it = iter_ident.as_ref().expect("for loop has an iterator");
            let pat = &f.pat;
            let benv = lenv.bind(pat_bindings(&f.pat));
            let (benv, step) = match &peeked {
                Some((name, it)) => {
                    let (peek, at) = (range_peek_fn(), range_at_fn());
                    let benv = benv.derive(name.clone(), it.clone(), quote! { #at(&#it) });
                    (benv, quote! { #peek(&#it) })
                }
                None => (benv, quote! { ::core::iter::Iterator::next(&mut #it) }),
            };
            let body = cps_block(ctx, &benv, &f.body, &next)?;
            let exhausted = k(quote! { () })?;
            quote! {
                match #step {
                    ::core::option::Option::None => #exhausted,
                    ::core::option::Option::Some(#pat) => #body,
                }
            }
        }
        Expr::While(w) => {
            if let Expr::Let(l) = &*w.cond {
                if contains_rec(ctx, &l.expr) {
                    return Err(syn::Error::new(
                        l.span(),
                        "`#[stack_safe]` does not support a recursive call in a `while let` \
                         scrutinee",
                    ));
                }
                let scrutinee = leaf_expr(&lenv, &l.expr)?;
                let pat = &l.pat;
                let benv = lenv.bind(pat_bindings(&l.pat));
                let body = cps_block(ctx, &benv, &w.body, &next)?;
                let done = k(quote! { () })?;
                quote! {
                    match #scrutinee {
                        #pat => #body,
                        _ => #done,
                    }
                }
            } else {
                let body = cps_block(ctx, &lenv, &w.body, &next)?;
                let done = k(quote! { () })?;
                cps_expr(ctx, &lenv, &w.cond, &|c| {
                    Ok(quote! { if #c { #body } else { #done } })
                })?
            }
        }
        Expr::Loop(l) => cps_block(ctx, &lenv, &l.body, &next)?,
        _ => unreachable!("lower_loop is only called on loops"),
    };

    ctx.set_loop_body(idx, arm);

    // Entering the loop is a tail transfer: the entry point runs the loop and everything after.
    match (e, iter_ident) {
        (Expr::ForLoop(f), Some(it)) => match &store {
            // SAFETY: the pointer comes from `push_into`, and `Pin` never moves what it holds. The
            // value stays until this loop's mark is truncated, which only happens on leaving the
            // loop (`released_k` on exhaustion and `break`, `Env::teardown` on `?` and `return`;
            // not on `continue`). Only emitted under `data_in_frame`.
            Some((owner, held, mark, elem)) => {
                let slot = held.slot();
                let push = push_into(held, &root_expr(owner), &|_| None);
                Ok(quote! {
                {
                    let #mark = #ctxp.#slot.mark();
                    let __ss_owned = #push;
                    // Named because nothing outside the body builds this payload slot.
                    let mut #it: <&#elem as ::core::iter::IntoIterator>::IntoIter =
                        ::core::iter::IntoIterator::into_iter(unsafe { &*__ss_owned });
                    #enter
                }
                })
            }
            None => cps_expr(ctx, env, &f.expr, &|iter_val| {
                Ok(quote! {
                    {
                        let mut #it = ::core::iter::IntoIterator::into_iter(#iter_val);
                        #enter
                    }
                })
            }),
        },
        _ => Ok(enter),
    }
}

/// Is this `a..b` with both ends? That always builds a `core::ops::Range`, so `start` is readable.
fn is_bounded_range(e: &Expr) -> bool {
    match e {
        Expr::Range(r) => {
            matches!(r.limits, syn::RangeLimits::HalfOpen(_))
                && r.start.is_some()
                && r.end.is_some()
        }
        Expr::Paren(p) => is_bounded_range(&p.expr),
        Expr::Group(g) => is_bounded_range(&g.expr),
        _ => false,
    }
}

/// The name bound by a plain immutable by-value pattern.
fn plain_binding(pat: &Pat) -> Option<Ident> {
    match pat {
        Pat::Ident(p)
            if p.by_ref.is_none()
                && p.mutability.is_none()
                && p.subpat.is_none()
                && pat_bindings(pat).as_slice() == [p.ident.clone()] =>
        {
            Some(p.ident.clone())
        }
        _ => None,
    }
}

/// `name` as an expression.
fn root_expr(name: &Ident) -> Expr {
    parse_quote! { #name }
}

/// Push `value` into the store `held` names and return a pointer to it, or to the place
/// `project` selects inside it (shared store only, via `Pin::push_projected`).
fn push_into(held: &Held, value: &Expr, project: &dyn Fn(&Ident) -> Option<Expr>) -> Expr {
    let ctxp = ctx_param();
    let slot = held.slot();
    match held.variant() {
        None => parse_quote! { #ctxp.#slot.push(#value) },
        Some(v) => {
            let (en, var) = (pinned_ty(), pinned_variant(v));
            let owned = format_ident!("__ss_held");
            let inner = match project(&owned) {
                Some(place) => place,
                None => parse_quote! { #owned },
            };
            parse_quote! {
                #ctxp.#slot.push_projected(#en::#var(#value), |__ss_d| match __ss_d {
                    #en::#var(#owned) => #inner,
                    #[allow(unreachable_patterns)]
                    _ => ::core::unreachable!("the store holds what this push put in it"),
                })
            }
        }
    }
}

/// Take a parked local back out of the store at `mark + at`, dropping anything above it.
/// Panics if the variant is wrong.
fn take_back(held: &Held, root: &Ident, mark: &Ident, at: usize) -> TokenStream {
    let ctxp = ctx_param();
    let slot = held.slot();
    let at = syn::Index::from(at);
    let taken = quote! { #ctxp.#slot.take_at(#mark + #at).expect("parked by this frame") };
    match held.variant() {
        None => quote! { let #root = #taken; },
        Some(v) => {
            let (en, var) = (pinned_ty(), pinned_variant(v));
            quote! {
                let #root = match #taken {
                    #en::#var(__ss_v) => __ss_v,
                    #[allow(unreachable_patterns)]
                    _ => ::core::unreachable!("the store hands back what this frame parked"),
                };
            }
        }
    }
}

/// Rewrite a lent place to go through the parked copy: `&def.body` becomes `&owned.body`.
fn project_from(place: &Expr, root: &Ident, owned: &Ident) -> Expr {
    struct V<'a> {
        root: &'a Ident,
        owned: &'a Ident,
    }

    impl V<'_> {
        /// Does this block rebind the root? Then mentions inside it are not the root.
        fn rebinds_root(&self, e: &Expr) -> bool {
            match e {
                Expr::Block(b) => b.block.stmts.iter().any(|s| match s {
                    Stmt::Local(l) => pat_bindings(&l.pat).iter().any(|b| b == self.root),
                    _ => false,
                }),
                Expr::Closure(c) => c
                    .inputs
                    .iter()
                    .any(|p| pat_bindings(p).iter().any(|b| b == self.root)),
                _ => false,
            }
        }
    }

    impl VisitMut for V<'_> {
        fn visit_expr_mut(&mut self, e: &mut Expr) {
            if let Expr::Path(p) = &*e
                && p.qself.is_none()
                && p.path.is_ident(self.root)
            {
                let owned = self.owned;
                *e = parse_quote! { #owned };
                return;
            }
            if self.rebinds_root(e) {
                return;
            }
            syn::visit_mut::visit_expr_mut(self, e);
        }
    }

    let mut out = place.clone();
    V { root, owned }.visit_expr_mut(&mut out);
    parse_quote! { &#out }
}
/// The local a loop iterator borrows, for `&xs` and `xs.iter()`.
fn borrowed_owner(e: &Expr) -> Option<Ident> {
    let path_ident = |e: &Expr| match e {
        Expr::Path(p) => p.path.get_ident().cloned(),
        _ => None,
    };
    match e {
        // Shared borrows only, so `&mut xs` still gets borrowck's error.
        Expr::Reference(r) if r.mutability.is_none() => path_ident(&r.expr),
        Expr::MethodCall(m) if m.method == "iter" && m.args.is_empty() => path_ident(&m.receiver),
        _ => None,
    }
}

/// Is this a compound assignment (`+=`, `<<=`, ...)?
fn is_assign_op(op: &syn::BinOp) -> bool {
    use syn::BinOp::*;
    matches!(
        op,
        AddAssign(_)
            | SubAssign(_)
            | MulAssign(_)
            | DivAssign(_)
            | RemAssign(_)
            | BitXorAssign(_)
            | BitAndAssign(_)
            | BitOrAssign(_)
            | ShlAssign(_)
            | ShrAssign(_)
    )
}

/// CPS subexpressions left to right, then call `k` with their values. A value followed by a
/// recursing one is bound to a temporary first, to keep side-effect order.
fn cps_seq(
    ctx: &Ctx,
    env: &Env,
    exprs: &[&Expr],
    acc: Vec<TokenStream>,
    k: &dyn Fn(&[TokenStream]) -> syn::Result<TokenStream>,
) -> syn::Result<TokenStream> {
    let Some((first, rest)) = exprs.split_first() else {
        return k(&acc);
    };

    // Operands move out of their written positions, where a `#[cfg]` can't follow.
    reject_cfg(
        &expr_attrs(first),
        "an operand of an expression that recurses",
    )?;

    let rest_recurses = rest.iter().any(|e| contains_rec(ctx, e));

    if !contains_rec(ctx, first) && !rest_recurses {
        let mut acc = acc;
        acc.push(leaf_expr(env, first)?);
        return cps_seq(ctx, env, rest, acc, k);
    }

    if !contains_rec(ctx, first) {
        let tmp = ctx.fresh();
        if let Some(ty) = ctx.type_of(first) {
            ctx.note_local_type(&tmp, ty);
        }
        let head = leaf_expr(env, first)?;
        let inner = env.bind([tmp.clone()]);
        let mut acc = acc;
        acc.push(quote! { #tmp });
        let tail = cps_seq(ctx, &inner, rest, acc, k)?;
        return Ok(quote! { { let #tmp = #head; #tail } });
    }

    cps_expr(ctx, env, first, &|v| {
        let mut acc = acc.clone();
        acc.push(v);
        cps_seq(ctx, env, rest, acc, k)
    })
}
