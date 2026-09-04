// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Rewriting code that contains no recursive call. It is spliced verbatim, but
//! it now lives inside a closure returning `__SsStep`, so `?`, `return`, and —
//! inside a lowered loop — `break` and `continue` still have to be adjusted.

use proc_macro2::{Ident, Span, TokenStream};
use quote::{ToTokens, quote};
use syn::spanned::Spanned;
use syn::visit_mut::VisitMut;
use syn::{Expr, Stmt, parse_quote};

use super::names::{entry_ty, entry_variant, state_marker, step_ty};
use super::try_shim;
use super::walk::{Env, LoopCtx};

struct LeafRewrite<'a> {
    /// Emitted before every escape; see `Env::restores`.
    restores: TokenStream,
    /// Emitted before `?` and `return` only; see `Env::teardown`.
    teardown: TokenStream,
    /// How this member's result enters its group's union; see `Env::wrap`.
    wrap: Option<(Ident, Ident)>,
    /// Innermost lowered loop, if any: the target for `break` / `continue`.
    lp: Option<&'a LoopCtx<'a>>,
    /// Nesting depth of ordinary (non-lowered) loops, whose `break` and
    /// `continue` belong to themselves.
    depth: usize,
    err: Option<syn::Error>,
}

impl LeafRewrite<'_> {
    /// A value as this member's result, wrapped into the union if there is one.
    fn wrapped(&self, v: TokenStream) -> TokenStream {
        match &self.wrap {
            None => v,
            Some((union, variant)) => quote! { #union::#variant(#v) },
        }
    }

    fn fail(&mut self, span: Span, msg: &str) {
        if self.err.is_none() {
            self.err = Some(syn::Error::new(span, msg));
        }
    }
}

impl VisitMut for LeafRewrite<'_> {
    fn visit_expr_mut(&mut self, e: &mut Expr) {
        match e {
            // These have their own `return` / `?` / loop targets.
            Expr::Closure(_) | Expr::Async(_) | Expr::Const(_) => return,
            Expr::ForLoop(_) | Expr::While(_) | Expr::Loop(_) => {
                self.depth += 1;
                syn::visit_mut::visit_expr_mut(self, e);
                self.depth -= 1;
                return;
            }
            _ => {}
        }
        syn::visit_mut::visit_expr_mut(self, e);

        let step = step_ty();
        let entry = entry_ty();
        // Undo a pending swap before leaving; empty in every other position.
        let undo = &self.restores;
        // Release a loop's store, but only where the member itself is abandoned.
        let release = &self.teardown;
        match e {
            Expr::Try(t) => {
                let inner = &t.expr;
                let branch = try_shim::branch(quote! { #inner });
                let exit = self.wrapped(try_shim::from_residual(quote! { __ss_res }));
                *e = parse_quote! {
                    match #branch {
                        ::core::result::Result::Ok(__ss_ok) => __ss_ok,
                        ::core::result::Result::Err(__ss_res) => {
                            #undo
                            #release
                            return #step::Done(#exit)
                        }
                    }
                };
            }
            Expr::Return(r) => {
                let raw = match &r.expr {
                    Some(v) => quote! { #v },
                    None => quote! { () },
                };
                // Bound before the teardown: the value may read out of a store it releases, as
                // `return Err(*x)` does for an `x` borrowed from one.
                let v = self.wrapped(quote! { __ss_ret });
                *e = parse_quote! {
                    { let __ss_ret = #raw; #undo #release return #step::Done(#v) }
                };
            }
            Expr::Continue(c) if self.depth == 0 => {
                if c.label.is_some() {
                    self.fail(
                        c.span(),
                        "`#[stack_safe]` does not support labelled `continue` in a loop that \
                         contains a recursive call",
                    );
                    return;
                }
                if let Some(lp) = self.lp {
                    let v = entry_variant(lp.variant);
                    let marker = state_marker(lp.idx);
                    *e = parse_quote! { { #undo return #step::Tail(#entry::#v(#marker)) } };
                }
            }
            Expr::Break(b) if self.depth == 0 => {
                if b.label.is_some() {
                    self.fail(
                        b.span(),
                        "`#[stack_safe]` does not support labelled `break` in a loop that \
                         contains a recursive call",
                    );
                    return;
                }
                if let Some(lp) = self.lp {
                    // `b.expr` has already been rewritten by the recursion above.
                    let v = match &b.expr {
                        Some(v) => v.to_token_stream(),
                        None => quote! { () },
                    };
                    match (lp.brk)(v) {
                        Ok(after) => *e = parse_quote! { { #undo return #after } },
                        Err(err) => {
                            if self.err.is_none() {
                                self.err = Some(err);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn visit_item_mut(&mut self, _: &mut syn::Item) {}
}

fn rewrite_leaf<T>(env: &Env, node: &mut T) -> syn::Result<()>
where
    for<'a> LeafRewrite<'a>: VisitMutOn<T>,
{
    let mut r = LeafRewrite {
        restores: env.restores.clone(),
        teardown: env.teardown.clone(),
        wrap: env.wrap.clone(),
        lp: env.lp,
        depth: 0,
        err: None,
    };
    r.apply(node);
    match r.err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Tiny helper so `rewrite_leaf` works for both `Expr` and `Stmt`.
trait VisitMutOn<T> {
    fn apply(&mut self, node: &mut T);
}
impl VisitMutOn<Expr> for LeafRewrite<'_> {
    fn apply(&mut self, node: &mut Expr) {
        self.visit_expr_mut(node);
    }
}
impl VisitMutOn<Stmt> for LeafRewrite<'_> {
    fn apply(&mut self, node: &mut Stmt) {
        self.visit_stmt_mut(node);
    }
}

/// Tokens for an expression containing no recursive call.
pub(super) fn leaf_expr(env: &Env, e: &Expr) -> syn::Result<TokenStream> {
    let mut e = e.clone();
    rewrite_leaf(env, &mut e)?;
    Ok(e.to_token_stream())
}

pub(super) fn leaf_stmt(env: &Env, s: &Stmt) -> syn::Result<TokenStream> {
    let mut s = s.clone();
    rewrite_leaf(env, &mut s)?;
    Ok(s.to_token_stream())
}
