// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The CPS transform itself: every recursive call becomes a `Call` step plus a frame — the
//! defunctionalized continuation, one variant per call site carrying the locals live across
//! it — and a loop whose body recurses becomes a new entry point.

use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::visit_mut::VisitMut;
use syn::{Block, Expr, Pat, Stmt, parse_quote};

use super::analyze::{borrows_a_built_value, contains_rec, pat_bindings, stmt_contains_rec};
use super::context::{CtxArg, classify_ctx_arg};
use super::leaf::{leaf_expr, leaf_stmt};
use super::names::*;
use super::try_shim;
use super::walk::{Cont, Ctx, Env, LoopCtx};

fn cps_block(ctx: &Ctx, env: &Env, block: &Block, k: Cont) -> syn::Result<TokenStream> {
    cps_stmts(ctx, env, &block.stmts, k)
}

pub(super) fn cps_stmts(ctx: &Ctx, env: &Env, stmts: &[Stmt], k: Cont) -> syn::Result<TokenStream> {
    let Some((first, rest)) = stmts.split_first() else {
        return k(quote! { () });
    };

    if !stmt_contains_rec(ctx, first) {
        // A trailing expression without a semicolon is the block's value. A
        // *block-like* statement (`if c { .. }`) also parses as
        // `Stmt::Expr(_, None)` even with statements after it.
        if let Stmt::Expr(e, None) = first
            && rest.is_empty()
        {
            return k(leaf_expr(env, e)?);
        }
        let head = leaf_stmt(env, first)?;
        // A statement that leaves the block makes the continuation unreachable, so
        // it is not generated at all. That is not just a size saving: a lowered loop
        // generated only in dead code has nothing to pin its payload's type, and
        // rustc does not infer through unreachable code — the user would get
        // `type annotations needed` pointing into their own body.
        if diverges(first) {
            return Ok(quote! { { #head } });
        }
        // Bindings introduced here are visible to everything that follows,
        // including any loop that needs to thread them.
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
            let pat = &local.pat;
            let attrs = &local.attrs;
            cps_expr(ctx, env, &init.expr, &|v| {
                let inner = env.bind(pat_bindings(&local.pat));
                let tail = cps_stmts(ctx, &inner, rest, k)?;
                Ok(quote! { { #(#attrs)* let #pat = #v; #tail } })
            })
        }
        Stmt::Expr(e, Some(_)) => cps_expr(ctx, env, e, &|v| {
            let tail = cps_stmts(ctx, env, rest, k)?;
            Ok(quote! { { let _ = #v; #tail } })
        }),
        Stmt::Expr(e, None) if rest.is_empty() => cps_expr(ctx, env, e, k),
        Stmt::Expr(e, None) => cps_expr(ctx, env, e, &|v| {
            let tail = cps_stmts(ctx, env, rest, k)?;
            Ok(quote! { { let _ = #v; #tail } })
        }),
        Stmt::Item(_) | Stmt::Macro(_) => unreachable!("checked by stmt_contains_rec / validate"),
    }
}

/// Pull the *value* subexpressions out of a place so its side effects can run where
/// the source put them, leaving a place whose evaluation is just projections.
///
/// `&mut t.kids[idx()]` becomes `let __ss_vN = idx();` plus `&mut t.kids[__ss_vN]`.
/// The pointer itself still has to be taken last — user code must not run between
/// the derived pointer being created and the callee using it — but with the side
/// effects hoisted, "last" is no longer observable.
///
/// The root is left alone: it names the context, and hoisting it would copy or move
/// the very reference being projected from.
fn hoist_place(ctx: &Ctx, place: &Expr) -> (Vec<TokenStream>, Expr) {
    struct H<'a> {
        ctx: &'a Ctx,
        pre: Vec<TokenStream>,
    }

    impl H<'_> {
        fn take(&mut self, e: &mut Expr) {
            // A path or a literal has nothing to run; anything else might.
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
                // Children first, so nested projections hoist in evaluation order.
                Expr::Index(i) => {
                    self.visit_expr_mut(&mut i.expr);
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
                // Anything else is left whole: it is either the root or a shape the
                // place walk in `place_root` would not have accepted.
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

/// Is this expression a place whose evaluation cannot run user code?
///
/// A path, a field of one, or a dereference of one. An index is *not* included: the index
/// expression may have side effects, and those have to keep their position, so such a
/// receiver goes through the ordinary hoisting.
fn is_simple_place(e: &Expr) -> bool {
    match e {
        Expr::Path(p) => p.qself.is_none(),
        Expr::Field(f) => is_simple_place(&f.base),
        Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => is_simple_place(&u.expr),
        Expr::Paren(p) => is_simple_place(&p.expr),
        Expr::Group(g) => is_simple_place(&g.expr),
        _ => false,
    }
}

/// Does this statement leave the enclosing block unconditionally? Only the shapes
/// that plainly do — a bare `return`, `break` or `continue` — which is enough for the
/// continuation duplication that branching produces.
fn diverges(stmt: &Stmt) -> bool {
    let Stmt::Expr(e, _) = stmt else { return false };
    matches!(e, Expr::Return(_) | Expr::Break(_) | Expr::Continue(_))
}

fn cps_expr(ctx: &Ctx, env: &Env, e: &Expr, k: Cont) -> syn::Result<TokenStream> {
    // The base case that makes this tractable: anything with no recursive call
    // is just an expression, spliced into the continuation.
    if !contains_rec(ctx, e) {
        return k(leaf_expr(env, e)?);
    }

    let step = step_ty();
    let entry = entry_ty();
    let frame = frame_ty();

    // ---- the recursive call itself --------------------------------------
    // Checked before the match so the callee and its arguments are named once.
    if let Some((callee, call)) = ctx.rec_call(e) {
        let callee_variant = entry_variant(callee);
        // Context arguments do not travel in the payload. Either the child
        // shares the parent's slot, in which case there is nothing to pass,
        // or it works on a place derived from it, in which case the slot is
        // swapped for the child and restored by the continuation.
        let mut payload: Vec<Expr> = Vec::new();
        let mut swaps: Vec<(usize, Expr)> = Vec::new();
        let ctxp = ctx_param();
        // A pinned position is where this call lends the callee a value built here: the
        // value moves into the driver's store for that position, which keeps it at a
        // fixed address, and the pointer travels in the payload. Anything else at such a
        // position becomes a pointer too, since the payload has one type.
        //
        // One store per position, so positions of different types do not have to agree.
        let mut pinned_slots: Vec<syn::Index> = Vec::new();
        for (i, arg) in call.args.iter().enumerate() {
            match ctx.member(callee).context_at.get(&i) {
                None => {
                    let j = payload.len();
                    if ctx.member(callee).pinned[j].get() {
                        let pin = ctx.pin_slot(callee, j);
                        payload.push(match borrows_a_built_value(arg) {
                            Some(built) => {
                                if !pinned_slots.contains(&pin) {
                                    pinned_slots.push(pin.clone());
                                }
                                parse_quote! { #ctxp.#pin.push(#built) }
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
        // One mark per store this call pushes into, taken before any argument is
        // evaluated so that it covers every push. Fresh names rather than ones keyed to
        // the resume index, because an argument may itself contain a call and reserve a
        // point first.
        let marks: Vec<(syn::Index, Ident)> = pinned_slots
            .iter()
            .map(|slot| (slot.clone(), ctx.fresh()))
            .collect();
        let out = cps_seq(ctx, env, &payload, Vec::new(), &|vals| {
            let v = ctx.fresh();
            let mut saved: Vec<Ident> = swaps.iter().map(|(slot, _)| saved_slot(*slot)).collect();
            // The mark has to be carried whether or not the continuation mentions it,
            // exactly like a parked pointer, so it is forced into the payload.
            for (_, mark) in &marks {
                saved.push(mark.clone());
            }

            // Reserve the resume point before generating its code, so a nested call
            // inside the continuation gets a later index.
            //
            // Its payload is solved from the same scope a loop's would be: the
            // bindings in scope at the call, plus the parked pointers, minus whatever
            // the resume code turns out not to mention.
            let r =
                ctx.reserve_resume(ctx.scope_with_results(&env.scope), saved.clone(), v.clone());
            let frame_var = frame_variant(r);
            let marker = frame_marker(r);

            // `v` is in scope for everything the continuation contains, including any
            // loop it lowers, which has to thread it.
            let body = ctx.with_result(v.clone(), || k(quote! { #v }))?;
            let prologue = ctx.ctx_prologue();

            // A resumed value arrives in the union when the members' return types differ,
            // and the callee is known here, so the variant is too.
            let unwrap = ctx.unwrap_result(callee, &v);

            // Restoring a parked pointer has to happen *before* the prologue derives
            // the context bindings from it, or they would be the child's.
            let restores = swaps.iter().map(|(slot, _)| {
                let (saved, idx) = (saved_slot(*slot), syn::Index::from(*slot));
                quote! { #ctxp.#idx = #saved; }
            });
            // Whatever this call lent the callee dies with the callee, i.e. now.
            let unpin = marks
                .iter()
                .map(|(slot, mark)| quote! { #ctxp.#slot.truncate(#mark); });
            let unpin = quote! { #(#unpin)* };
            ctx.set_resume_code(
                r,
                quote! {
                    #unpin
                    #(#restores)*
                    #prologue
                    #unwrap
                    #body
                },
            );

            // Without a swap the arguments stay inline, so the common path gains
            // no bindings at all.
            let call = if swaps.is_empty() {
                quote! {
                    #step::Call(
                        #entry::#callee_variant((#(#vals,)*)),
                        #frame::#frame_var(#marker),
                    )
                }
            } else {
                // With a swap, every argument is bound *in source order* first and
                // the derived pointers are taken last. Taking a pointer earlier is
                // not an option: user code running between its creation and the
                // callee's use of it is either a foreign write to it (UB) or an
                // escape that skips the restore. Hoisting the side effects out of
                // the place is what keeps the observable order the source's.
                // Park each parent pointer in a local — it is `Copy`, so it just
                // rides in the frame — and take the derived one from the place.
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

                // The derived pointer is taken *where the source takes it*. Borrowck
                // is what makes that safe: an argument written after a `&mut` place
                // cannot touch the parent — `f(&mut b[0], b.len())` is E0502 — so
                // nothing between taking the pointer and the callee's use of it can
                // invalidate it. An argument that *escapes* is handled by giving it
                // the restores to run first (`Env::restores`). Only an argument that
                // recurses — possible with a *shared* context, where several borrows
                // coexist — still forces the swap to the end, because its own frames
                // would outlive the window.
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
                                // Holds a pointer into the pinned store, not the
                                // parameter's own reference type.
                                TokenStream::new()
                            } else {
                                ctx.member(callee)
                                    .param_types
                                    .get(payload_seen)
                                    .cloned()
                                    .unwrap_or_default()
                            };
                            // Re-evaluated here rather than reused from `cps_seq`, so that an
                            // escape inside it can be given the restores of any swap already
                            // performed. From `payload` rather than from `call.args`: that is
                            // the argument as the driver needs it, in particular a value built
                            // here moved into the pinned store instead of merely referenced.
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

                quote! {
                    #(#pre)*
                    #(#deferred)*
                    #step::Call(
                        #entry::#callee_variant((#(#held,)*)),
                        #frame::#frame_var(#marker),
                    )
                }
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
                    // A guard is part of the pattern now: `Pat::Guard { pat, guard }`.
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
                    let body = cps_expr(ctx, &inner, &arm.body, k)?;
                    // Attributes travel with the arm. Dropping a `#[cfg]` would leave a gated
                    // arm in beside its twin, shadowing whatever fell through to it.
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
                Ok(quote! { return #step::Done(#v) })
            })
        }

        Expr::Try(t) => cps_expr(ctx, env, &t.expr, &|v| {
            let ok = ctx.fresh();
            // `ok` is a binding the transform introduces, so nothing in the user's
            // scope names it. A later call in the same expression still has to carry
            // it, as in `f(..)? + g(..)?`, hence recording it as a live result.
            let body = ctx.with_result(ok.clone(), || k(quote! { #ok }))?;
            let branch = try_shim::branch(v.clone());
            let exit = env.wrapped(try_shim::from_residual(quote! { __ss_res }));
            Ok(quote! {
                match #branch {
                    ::core::result::Result::Ok(#ok) => #body,
                    ::core::result::Result::Err(__ss_res) => {
                        return #step::Done(#exit)
                    }
                }
            })
        }),

        // ---- loops ------------------------------------------------------
        Expr::ForLoop(_) | Expr::While(_) | Expr::Loop(_) => lower_loop(ctx, env, e, k),

        // `break` / `continue` reached through CPS (the surrounding code
        // recurses), as opposed to through leaf rewriting.
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
                    Ok(quote! { #step::Tail(#entry::#v(#marker)) })
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

        // Compound assignment. The left operand is a *place*, not a value, so it
        // must not be hoisted into a temporary the way a strict operand would be.
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
        Expr::Index(i) => cps_seq(ctx, env, &[&i.expr, &i.index], Vec::new(), &|v| {
            let (b, idx) = (&v[0], &v[1]);
            k(quote! { (#b)[#idx] })
        }),
        Expr::Tuple(t) => {
            let elems: Vec<&Expr> = t.elems.iter().collect();
            cps_seq(ctx, env, &elems, Vec::new(), &|v| k(quote! { (#(#v,)*) }))
        }
        Expr::Array(a) => {
            let elems: Vec<&Expr> = a.elems.iter().collect();
            cps_seq(ctx, env, &elems, Vec::new(), &|v| k(quote! { [#(#v),*] }))
        }
        Expr::Call(call) => {
            let func = &call.func;
            let args: Vec<&Expr> = call.args.iter().collect();
            cps_seq(ctx, env, &args, Vec::new(), &|v| {
                k(quote! { #func(#(#v),*) })
            })
        }
        Expr::MethodCall(mc) => {
            let method = &mc.method;
            let turbofish = &mc.turbofish;
            // A receiver that is a plain *place* is left where it is. Hoisting it would
            // bind it by value, so a method taking `&mut self` would mutate the copy and
            // the original would answer afterwards — silently, whenever the receiver is
            // `Copy`. Reading a place has no side effect, so leaving it to the
            // continuation is not observable either way.
            if is_simple_place(&mc.receiver) && !contains_rec(ctx, &mc.receiver) {
                let recv = &mc.receiver;
                let args: Vec<&Expr> = mc.args.iter().collect();
                return cps_seq(ctx, env, &args, Vec::new(), &|v| {
                    k(quote! { (#recv).#method #turbofish (#(#v),*) })
                });
            }
            let mut parts: Vec<&Expr> = vec![&mc.receiver];
            parts.extend(mc.args.iter());
            cps_seq(ctx, env, &parts, Vec::new(), &|v| {
                let recv = &v[0];
                let args = &v[1..];
                k(quote! { (#recv).#method #turbofish (#(#args),*) })
            })
        }
        Expr::Struct(s) => {
            let path = &s.path;
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
                    // The fields above already emit a trailing comma each, so this
                    // must not add another: `S { v: x, , ..b }` does not parse.
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

/// Lower a loop whose body recurses into a fresh entry point.
///
/// One iteration becomes `Tail(En(state))`: re-enter the body at the loop's entry
/// point without pushing a frame, so iterating costs no stack. The loop's state —
/// iterator plus the locals live across it — travels in the entry payload.
fn lower_loop(ctx: &Ctx, env: &Env, e: &Expr, k: Cont) -> syn::Result<TokenStream> {
    let step = step_ty();
    let entry = entry_ty();
    let ctxp = ctx_param();

    let iter_ident = match e {
        Expr::ForLoop(_) => Some(format_ident!("__ss_it{}", ctx.loops.borrow().len())),
        _ => None,
    };

    // A `for` loop over a borrow moves its collection into the store first: the iterator is
    // parked in the payload, and one over `&local` would borrow what that payload owns.
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
            let slot = ctx.loop_store_slot(ctx.loops.borrow().len(), elem.clone());
            Some((owner, slot, ctx.fresh(), elem))
        }
    };
    let store_forced: Vec<Ident> = store.iter().map(|(_, _, mark, _)| mark.clone()).collect();
    // Not an `Env::restores`: that also runs on `continue`, which still needs the collection.
    let release = match &store {
        Some((_, slot, mark, _)) => quote! { #ctxp.#slot.truncate(#mark); },
        None => TokenStream::new(),
    };

    let idx = ctx.reserve_loop(
        ctx.scope_with_results(&env.scope),
        iter_ident.clone(),
        store_forced,
    );
    let variant = entry_variant(ctx.loop_base() + idx);
    let marker = state_marker(idx);

    // Leaving the loop releases the store; `?` and `return` go through `Env::teardown`.
    // The value may come out of the store, so it is bound before the store is released.
    let released_k = |v: TokenStream| -> syn::Result<TokenStream> {
        if release.is_empty() {
            return k(v);
        }
        let after = k(quote! { __ss_left_loop })?;
        let release = &release;
        Ok(quote! { { let __ss_left_loop = #v; #release #after } })
    };
    let k: Cont = &released_k;

    // Inside the loop, `continue` re-enters this entry point and `break` runs
    // the code that follows the loop.
    let lp = LoopCtx {
        idx,
        variant: ctx.loop_base() + idx,
        brk: k,
    };
    // The iterator is a binding inside the loop's entry point, so a *nested*
    // loop must be able to thread it onward — otherwise this loop could not
    // resume once the inner one finished.
    // A binding of the loop's entry point like the iterator: a resume point inside the body
    // re-enters the loop and must thread it onward.
    let store_bindings: Vec<Ident> = store.iter().map(|(_, _, mark, _)| mark.clone()).collect();
    let lenv = env
        .with_teardown(release.clone())
        .in_loop(&lp)
        .bind(iter_ident.clone())
        .bind(store_bindings);
    let again = quote! { #step::Tail(#entry::#variant(#marker)) };
    // The body's value is discarded, but it must still be *evaluated*: a branch
    // with no recursive call arrives here as a whole expression rather than as
    // statements already emitted, so dropping it would drop its side effects.
    let next =
        |v: TokenStream| -> syn::Result<TokenStream> { Ok(quote! { { let _ = #v; #again } }) };

    let arm = match e {
        Expr::ForLoop(f) => {
            let it = iter_ident.as_ref().expect("for loop has an iterator");
            let pat = &f.pat;
            let benv = lenv.bind(pat_bindings(&f.pat));
            let body = cps_block(ctx, &benv, &f.body, &next)?;
            let exhausted = k(quote! { () })?;
            quote! {
                match ::core::iter::Iterator::next(&mut #it) {
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

    // Entering the loop is also a tail transfer: the entry point computes the
    // loop *and* everything after it, which is exactly the rest of this frame.
    match (e, iter_ident) {
        (Expr::ForLoop(f), Some(it)) => match &store {
            // SAFETY: this lands in the caller's crate and the invariant is ours. The pointer is
            // one `Pin::push` returned for a value moved into the driver's store, and `Pin` never
            // moves a value it holds, so the address is good for as long as it is there. It is
            // there until this loop's mark is truncated, which happens on the way out of the loop
            // and nowhere else: `released_k` covers the exhausted branch and every `break`,
            // `Env::teardown` covers `?` and `return`, and `continue` deliberately does not
            // release. So every iteration that reads the iterator runs while the value is still
            // owned by the store. Gated behind `data_in_frame`.
            Some((owner, slot, mark, elem)) => Ok(quote! {
                {
                    let #mark = #ctxp.#slot.mark();
                    let __ss_owned = #ctxp.#slot.push(#owner);
                    // The iterator's type is named because its payload slot is only built
                    // inside the closure. The shape is enough; regionck settles the lifetime.
                    let mut #it: <&#elem as ::core::iter::IntoIterator>::IntoIter =
                        ::core::iter::IntoIterator::into_iter(unsafe { &*__ss_owned });
                    #step::Tail(#entry::#variant(#marker))
                }
            }),
            None => cps_expr(ctx, env, &f.expr, &|iter_val| {
                Ok(quote! {
                    {
                        let mut #it = ::core::iter::IntoIterator::into_iter(#iter_val);
                        #step::Tail(#entry::#variant(#marker))
                    }
                })
            }),
        },
        _ => Ok(quote! { #step::Tail(#entry::#variant(#marker)) }),
    }
}

/// The local a loop's iterator borrows, for the two forms that name a place: `&xs` and
/// `xs.iter()`. Anything else either owns what it yields or borrows something unidentifiable.
fn borrowed_owner(e: &Expr) -> Option<Ident> {
    let path_ident = |e: &Expr| match e {
        Expr::Path(p) => p.path.get_ident().cloned(),
        _ => None,
    };
    match e {
        // Shared borrows only: the store lends through `&*ptr`, so leaving `&mut xs` alone keeps
        // the borrow-checker error naming the loop.
        Expr::Reference(r) if r.mutability.is_none() => path_ident(&r.expr),
        Expr::MethodCall(m) if m.method == "iter" && m.args.is_empty() => path_ident(&m.receiver),
        _ => None,
    }
}

/// Is this a compound assignment (`+=`, `<<=`, ...)? Its left operand is a place.
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

/// CPS a list of subexpressions in left-to-right evaluation order, then hand the
/// resulting value tokens to `k`.
///
/// A subexpression with no recursive call is normally spliced straight through,
/// but if anything *after* it recurses, it must be bound to a temporary first —
/// otherwise it would be moved into a continuation and evaluated after the
/// recursive call, reordering side effects.
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

    let rest_recurses = rest.iter().any(|e| contains_rec(ctx, e));

    if !contains_rec(ctx, first) && !rest_recurses {
        let mut acc = acc;
        acc.push(leaf_expr(env, first)?);
        return cps_seq(ctx, env, rest, acc, k);
    }

    if !contains_rec(ctx, first) {
        let tmp = ctx.fresh();
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
