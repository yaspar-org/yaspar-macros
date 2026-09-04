// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The passes that run before any code is generated: normalising a method's
//! `self`, deciding which context slots need a raw pointer, and rejecting what
//! the transform cannot rewrite.

use proc_macro2::{Ident, Span, TokenStream, TokenTree};
use quote::format_ident;
use std::collections::HashMap;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::visit_mut::VisitMut;
use syn::{Block, Expr, ItemFn, Pat, PatIdent, Stmt, parse_quote};

use super::context::{CtxArg, classify_ctx_arg, is_context_slot, strip_parens};
use super::names::self_binding;
use super::walk::Ctx;

// ---------------------------------------------------------------------------
// Method normalisation
// ---------------------------------------------------------------------------

/// What a method is split into, so that the transform never sees a receiver.
pub(super) struct MethodSplit {
    /// The method as the caller still sees it, its body reduced to one call.
    pub(super) outer: ItemFn,
    /// The name to emit the transformed body under, beside the method.
    pub(super) inner: Ident,
}

/// Turn a method into a plain function of its receiver, plus a wrapper.
///
/// `self` is not special to the transform; it is special only to Rust's syntax. So a
/// method is rewritten into an associated function whose first parameter is an
/// ordinary `&Self` or `&mut Self`, and the method itself keeps its signature and
/// forwards to it:
///
/// ```text
/// fn len(&self) -> usize { .. tail.len() .. }
///
///   ->  fn len(&self) -> usize { Self::__ss_impl_len(self) }          // the wrapper
///       fn __ss_impl_len(__ss_self: &Self) -> usize { .. len(&tail) .. } // transformed
/// ```
///
/// Everything downstream then applies its usual rules: a `&Self` parameter is `Copy`
/// and travels in the payload, so the callee may be a *different* value of the same
/// type, as in `tail.len()`. A `&mut Self` parameter becomes a context slot, exactly
/// as a `&mut` parameter of a free function does, so recursing into a place derived
/// from it needs `use_nonlinear_mut` — the same rule, stated once.
///
/// The inner function must be an associated one rather than nested in the wrapper,
/// because a nested `fn` cannot name `Self` (`E0401`).
/// Rewrite each `impl Trait` parameter into a generic parameter with the same bounds.
///
/// Argument-position `impl Trait` *is* a generic parameter, so naming it turns a type the
/// transform could not write down — and therefore could not use to pin a payload slot — into one
/// it can. Callers are unaffected: the two forms differ only in that a named parameter can be
/// given by turbofish.
pub(super) fn desugar_apit(func: &mut ItemFn) {
    let mut fresh = 0usize;
    for arg in func.sig.inputs.iter_mut() {
        let syn::FnArg::Typed(pt) = arg else { continue };
        let syn::Type::ImplTrait(it) = &*pt.ty else {
            continue;
        };
        let name = format_ident!("__SsApit{}", fresh);
        fresh += 1;
        let bounds = it.bounds.clone();
        func.sig
            .generics
            .params
            .push(syn::GenericParam::Type(syn::TypeParam {
                attrs: vec![],
                ident: name.clone(),
                colon_token: Some(Default::default()),
                bounds,
                default: None,
            }));
        *pt.ty = syn::parse_quote! { #name };
    }
}

pub(super) fn desugar_receiver(
    func: &mut ItemFn,
    group: &[Ident],
) -> syn::Result<Option<MethodSplit>> {
    let receiver_kind = match func.sig.inputs.first() {
        Some(syn::FnArg::Receiver(recv)) => match &recv.kind {
            syn::ReceiverKind::Reference(_, lifetime, mutability) => {
                Some((lifetime.clone(), *mutability))
            }
            _ => {
                return Err(syn::Error::new(
                    recv.span(),
                    "`#[stack_safe]` does not support a by-value `self`: the receiver becomes an \
                     ordinary parameter of the transformed function, which the driver either \
                     lends out or carries in the payload, and it can do neither with an owned \
                     value",
                ));
            }
        },
        _ => None,
    };

    rewrite_self_calls(
        func,
        group,
        receiver_kind.as_ref().is_some_and(|(_, m)| m.is_some()),
    );

    let Some((lifetime, mutability)) = receiver_kind else {
        return Ok(None);
    };

    // The wrapper forwards every parameter by name, so each one has to have a name.
    let mut forwarded: Vec<Ident> = Vec::new();
    for arg in func.sig.inputs.iter().skip(1) {
        let syn::FnArg::Typed(pt) = arg else { continue };
        let Pat::Ident(PatIdent { ident, .. }) = &*pt.pat else {
            return Err(syn::Error::new(
                pt.pat.span(),
                "`#[stack_safe]` requires plain identifier parameters; bind the pattern \
                 inside the body instead",
            ));
        };
        forwarded.push(ident.clone());
    }

    let receiver = self_binding();
    let receiver_ty: syn::Type = parse_quote! { & #lifetime #mutability Self };
    let inner = format_ident!("__ss_impl_{}", func.sig.ident);

    // The wrapper keeps the method's own attributes and visibility: it is the item the
    // outside world still sees.
    let mut outer = ItemFn {
        attrs: std::mem::take(&mut func.attrs),
        vis: func.vis.clone(),
        modifiers: func.modifiers.clone(),
        sig: func.sig.clone(),
        block: parse_quote! { { Self::#inner(self #(, #forwarded)*) } },
    };
    outer.attrs.push(parse_quote! { #[inline] });
    func.vis = syn::Visibility::Inherited;

    // The receiver is now the first ordinary parameter.
    let rest: Vec<syn::FnArg> = func.sig.inputs.iter().skip(1).cloned().collect();
    func.sig.inputs = parse_quote! { #receiver: #receiver_ty #(, #rest)* };

    Ok(Some(MethodSplit { outer, inner }))
}

/// Put method-form recursion into the plain-call form the rest of the transform
/// understands, and rename every `self` to a binding that can be re-bound.
///
/// `tail.len()` becomes `len(&tail)` and `Self::len(tail)` becomes `len(tail)`, so the
/// receiver is simply the first argument. These names are only ever *recognised*: a
/// call the transform recognises turns into an entry into the driver, so nothing is
/// emitted that would have to resolve.
///
/// The reference is added because method syntax auto-refs and plain-call syntax does
/// not: `self.kids[i].bump()` has to become `bump(&mut self.kids[i])`. A receiver that
/// is already a reference simply gains a layer, which coerces away at the argument.
/// `self` itself is passed as it stands, so that a `&mut` receiver still reads as the
/// *same* slot rather than one derived from it.
fn rewrite_self_calls(func: &mut ItemFn, group: &[Ident], receiver_is_mut: bool) {
    struct V<'a> {
        /// Every member of the group, not just this function: a method's body calls
        /// its partners through `self` too, and those are entries into the same
        /// driver.
        group: &'a [Ident],
        receiver_is_mut: bool,
    }

    fn is_self(e: &Expr) -> bool {
        matches!(e, Expr::Path(p)
            if p.qself.is_none() && p.path.segments.len() == 1 && p.path.segments[0].ident == "self")
    }

    impl VisitMut for V<'_> {
        // Top-down: the recursive call is recognised before its `self` receiver
        // is renamed out from under the check.
        fn visit_expr_mut(&mut self, e: &mut Expr) {
            match e {
                Expr::MethodCall(m) if self.group.contains(&m.method) => {
                    let name = m.method.clone();
                    let recv = &m.receiver;
                    let args = m.args.iter();
                    let recv: Expr = if is_self(recv) {
                        parse_quote! { #recv }
                    } else if self.receiver_is_mut {
                        parse_quote! { &mut #recv }
                    } else {
                        parse_quote! { & #recv }
                    };
                    *e = parse_quote! { #name(#recv #(, #args)*) };
                }
                Expr::Call(c) => {
                    // `Self::walk(x, ..)`, the explicit form of the above.
                    if let Expr::Path(p) = &*c.func {
                        let segs = &p.path.segments;
                        let name = segs.last().map(|s| s.ident.clone());
                        let is_member = segs.len() == 2
                            && segs[0].ident == "Self"
                            && name.as_ref().is_some_and(|n| self.group.contains(n));
                        if is_member {
                            let name = name.expect("checked above");
                            let args = c.args.iter();
                            *e = parse_quote! { #name(#(#args),*) };
                        }
                    }
                }
                Expr::Path(p)
                    if p.qself.is_none()
                        && p.path.segments.len() == 1
                        && p.path.segments[0].ident == "self" =>
                {
                    let binding = self_binding();
                    *e = parse_quote! { #binding };
                }
                _ => {}
            }
            syn::visit_mut::visit_expr_mut(self, e);
        }

        fn visit_item_mut(&mut self, _: &mut syn::Item) {}
    }

    V {
        group,
        receiver_is_mut,
    }
    .visit_block_mut(&mut func.block);
}

// ---------------------------------------------------------------------------
// Which payload positions carry pinned data
// ---------------------------------------------------------------------------

/// Is this argument a reference to a value built right here?
///
/// Only shapes that plainly construct at run time. A `&` of a literal or a constant is
/// promoted to `'static` and needs nothing.
/// Rewrite every call to one of these functions into a call to its renamed twin.
///
/// Used to keep a checked-only copy of a body self-consistent: inside `f_orig`, a call to `g`
/// becomes a call to `g_orig`, so what the borrow checker sees is the original program throughout
/// rather than a mixture. Every shape a recursive call can take is covered — `g(..)`,
/// `self::g(..)`, `Self::g(..)` and `x.g(..)` — and unlike the scan this *does* descend into
/// nested items, since a function declared in the body is part of the same original.
pub(super) fn rename_calls(func: &mut ItemFn, renames: &HashMap<String, Ident>) {
    struct V<'a> {
        renames: &'a HashMap<String, Ident>,
    }

    impl V<'_> {
        fn rename(&self, name: &mut Ident) {
            if let Some(to) = self.renames.get(&name.to_string()) {
                *name = to.clone();
            }
        }
    }

    impl VisitMut for V<'_> {
        fn visit_expr_mut(&mut self, e: &mut Expr) {
            match e {
                Expr::Call(call) => {
                    if let Expr::Path(p) = &mut *call.func {
                        let segments = p.path.segments.len();
                        let qualified = segments == 2
                            && matches!(
                                p.path.segments[0].ident.to_string().as_str(),
                                "self" | "Self"
                            );
                        if segments == 1 || qualified {
                            let last = p.path.segments.last_mut().expect("non-empty path");
                            self.rename(&mut last.ident);
                        }
                    }
                }
                Expr::MethodCall(m) => self.rename(&mut m.method),
                _ => {}
            }
            syn::visit_mut::visit_expr_mut(self, e);
        }
    }

    V { renames }.visit_item_fn_mut(func);
}

/// The value a call lends the callee, when the argument is `&<something the caller owns>`.
///
/// A built value (`&Node::Cons(..)`, `&t.child(i)`) and a local (`&case`) are the same case: the
/// caller owns it, and it has to outlive a call that becomes a `return`. So both are moved into
/// the driver's store. A local is *moved*, so using it after the call is a move error — which is
/// the clear one, unlike the `E0515` that leaving it as a borrow produces.
pub(super) fn borrows_a_built_value<'e>(
    ctx: &Ctx,
    member: usize,
    arg: &'e Expr,
) -> Option<&'e Expr> {
    let Expr::Reference(r) = strip_parens(arg) else {
        return None;
    };
    let inner = strip_parens(&r.expr);
    let built = matches!(
        inner,
        Expr::Call(_) | Expr::MethodCall(_) | Expr::Struct(_) | Expr::Macro(_)
    ) || ctx.owns_named_local(member, inner);
    built.then_some(&*r.expr)
}

/// Mark every payload position some call passes a freshly built value's reference to.
///
/// Natively the temporary in `rec(n, &Node::Cons(v, rest))` lives to the end of the
/// enclosing statement, which spans the call. The transform turns that call into a
/// *return*, so the temporary would be gone before the callee ran. Under
/// `data_in_frame` the value is moved into the driver's pinned store instead, which
/// keeps it at a fixed address until the frame that owns it is popped, and the callee
/// reaches it through a raw pointer.
///
/// Without the flag this is a hard error rather than an `E0515` blamed on the
/// attribute.
pub(super) fn scan_pinned_args(ctx: &Ctx, block: &Block) -> syn::Result<()> {
    struct V<'a> {
        ctx: &'a Ctx,
        err: Option<syn::Error>,
    }

    impl<'ast> Visit<'ast> for V<'_> {
        fn visit_expr(&mut self, e: &'ast Expr) {
            if let Some((callee, call)) = self.ctx.rec_call(e) {
                let p = self.ctx.member(callee);
                let mut payload_seen = 0usize;
                for (i, arg) in call.args.iter().enumerate() {
                    if p.context_at.contains_key(&i) {
                        continue;
                    }
                    if borrows_a_built_value(self.ctx, self.ctx.current.get(), arg).is_some() {
                        if !self.ctx.opts.data_in_frame {
                            if self.err.is_none() {
                                self.err = Some(syn::Error::new(
                                    arg.span(),
                                    "`#[stack_safe]` cannot pass a reference to a value built \
                                     here: the recursive call becomes a `return`, so this \
                                     temporary would be dropped before the callee runs. Opt in \
                                     with `#[stack_safe(data_in_frame)]`, which moves the value \
                                     into the driver's own store for as long as the frame that \
                                     built it lives — see README.md for the invariant that asks \
                                     of you",
                                ));
                            }
                        } else if let Some(cell) = p.pinned.get(payload_seen) {
                            cell.set(true);
                        }
                    }
                    payload_seen += 1;
                }
            }
            syn::visit::visit_expr(self, e);
        }

        fn visit_item(&mut self, _: &'ast syn::Item) {}
    }

    let mut v = V { ctx, err: None };
    v.visit_block(block);
    match v.err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Which context slots need a raw pointer
// ---------------------------------------------------------------------------

/// Check every recursive call's arguments at context positions, and mark the
/// slots that a call passes a *derived* reference to.
///
/// A derived reference (`walk(&mut t.kids[i])`) means the child works on a
/// different place than its parent, so the slot cannot simply be shared: the
/// pointer has to be swapped for the child's subtree and restored afterwards.
/// A `&mut` cannot be parked like that — hence a raw pointer, hence the opt-in.
pub(super) fn scan_context_args(ctx: &Ctx, block: &Block) -> syn::Result<()> {
    struct V<'a> {
        ctx: &'a Ctx,
        err: Option<syn::Error>,
    }

    impl V<'_> {
        fn fail(&mut self, span: Span, msg: String) {
            if self.err.is_none() {
                self.err = Some(syn::Error::new(span, msg));
            }
        }
    }

    impl<'ast> Visit<'ast> for V<'_> {
        fn visit_expr(&mut self, e: &'ast Expr) {
            if let Some((callee, call)) = self.ctx.rec_call(e) {
                let callee = self.ctx.member(callee);
                // A mismatched arity has already been reported by `validate`, which
                // runs first; the slot-to-position mapping is meaningless here, so
                // there is nothing more worth saying about such a call.
                if call.args.len() == callee.arity {
                    for (i, arg) in call.args.iter().enumerate() {
                        let Some(&slot) = callee.context_at.get(&i) else {
                            continue;
                        };
                        let entry = &self.ctx.context[slot];
                        match classify_ctx_arg(arg, &self.ctx.context) {
                            Some(CtxArg::Same) => {}
                            Some(CtxArg::Derived(place)) => {
                                // The place is spliced into `ptr::from_mut(..)`
                                // verbatim, so a recursive call inside it would be
                                // left to recurse on the native stack — silently
                                // defeating the whole transform.
                                if contains_rec(self.ctx, &place) {
                                    self.fail(
                                        place.span(),
                                        format!(
                                            "`#[stack_safe]` cannot rewrite a recursive call \
                                             inside the place passed for `{}`: that place is \
                                             taken as a pointer before the call is made, so the \
                                             inner call would recurse on the native stack. Bind \
                                             it to a `let` before this call",
                                            entry.name
                                        ),
                                    );
                                } else if self.ctx.opts.use_nonlinear_mut {
                                    entry.raw.set(true);
                                } else {
                                    self.fail(
                                        arg.span(),
                                        format!(
                                            "`#[stack_safe]` cannot pass a reference derived from \
                                             `{}` to a recursive call: the parent frame keeps its \
                                             own reference alive, so the two cannot both be `&mut`. \
                                             Opt in with `#[stack_safe(use_nonlinear_mut)]`, \
                                             which parks the pointers instead — see README.md for \
                                             the invariant you take on",
                                            entry.name
                                        ),
                                    );
                                }
                            }
                            None => self.fail(
                                arg.span(),
                                format!(
                                    "`#[stack_safe]` requires a recursive call to pass `{0}` \
                                     itself (or `&mut *{0}`) in this position, or a place rooted \
                                     at a context parameter under \
                                     `#[stack_safe(use_nonlinear_mut)]`; anything else \
                                     could not outlive the call",
                                    entry.name
                                ),
                            ),
                        }
                    }
                }
                for a in &call.args {
                    self.visit_expr(a);
                }
                return;
            }
            syn::visit::visit_expr(self, e);
        }

        fn visit_item(&mut self, _: &'ast syn::Item) {}
    }

    let mut v = V { ctx, err: None };
    v.visit_block(block);
    match v.err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

pub(super) fn validate(ctx: &Ctx, block: &Block) -> syn::Result<()> {
    struct V<'a> {
        ctx: &'a Ctx,
        err: Option<syn::Error>,
    }

    impl V<'_> {
        fn fail(&mut self, span: Span, msg: &str) {
            if self.err.is_none() {
                self.err = Some(syn::Error::new(span, msg));
            }
        }

        fn check_macro(&mut self, mac: &syn::Macro, span: Span) {
            for name in self.ctx.names() {
                if tokens_mention(&mac.tokens, name) {
                    self.fail(
                        span,
                        &format!(
                            "possible recursive call to `{name}` inside a macro invocation; \
                             `#[stack_safe]` cannot rewrite macro bodies — bind the call to a \
                             `let` outside the macro",
                        ),
                    );
                    return;
                }
            }
        }
    }

    impl<'ast> Visit<'ast> for V<'_> {
        fn visit_expr(&mut self, e: &'ast Expr) {
            if let Some((callee, call)) = self.ctx.rec_call(e) {
                let p = self.ctx.member(callee);
                if call.args.len() != p.arity {
                    self.fail(
                        e.span(),
                        &format!(
                            "recursive call passes {} of `{}`'s {} parameters",
                            call.args.len(),
                            p.name,
                            p.arity
                        ),
                    );
                }
                for a in &call.args {
                    self.visit_expr(a);
                }
                return;
            }
            if let Expr::Path(path) = e
                && path.qself.is_none()
                && path.path.segments.len() == 1
            {
                let id = &path.path.segments[0].ident;
                if self.ctx.index_of(id).is_some() {
                    self.fail(
                        e.span(),
                        &format!(
                            "`{id}` is used as a value; `#[stack_safe]` can only rewrite \
                                 direct calls, not references to a function it transforms",
                        ),
                    );
                }
            }
            syn::visit::visit_expr(self, e);
        }

        fn visit_expr_macro(&mut self, m: &'ast syn::ExprMacro) {
            self.check_macro(&m.mac, m.span());
        }

        // A macro in *statement* position (`println!("{}", f(n - 1));`) is
        // `Stmt::Macro`, not `Expr::Macro`, so it needs its own visit. Missing it
        // meant the call was left untouched — `contains_rec` does not look inside
        // macro tokens either, so the whole statement was spliced in as a leaf and
        // recursed on the native stack, silently.
        fn visit_stmt_macro(&mut self, m: &'ast syn::StmtMacro) {
            self.check_macro(&m.mac, m.span());
        }

        fn visit_item(&mut self, _: &'ast syn::Item) {}
    }

    let mut v = V { ctx, err: None };
    v.visit_block(block);
    match v.err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

pub(super) fn tokens_mention(ts: &TokenStream, name: &Ident) -> bool {
    ts.clone().into_iter().any(|t| match t {
        TokenTree::Ident(i) => i == *name,
        TokenTree::Group(g) => tokens_mention(&g.stream(), name),
        _ => false,
    })
}

pub(super) fn contains_rec(ctx: &Ctx, e: &Expr) -> bool {
    struct V<'a> {
        ctx: &'a Ctx,
        found: bool,
    }
    impl<'ast> Visit<'ast> for V<'_> {
        fn visit_expr(&mut self, e: &'ast Expr) {
            if self.ctx.is_rec_call(e) {
                self.found = true;
                return;
            }
            syn::visit::visit_expr(self, e);
        }
        fn visit_item(&mut self, _: &'ast syn::Item) {}
    }
    let mut v = V { ctx, found: false };
    v.visit_expr(e);
    v.found
}

pub(super) fn stmt_contains_rec(ctx: &Ctx, s: &Stmt) -> bool {
    match s {
        Stmt::Local(l) => l.init.as_ref().is_some_and(|i| {
            contains_rec(ctx, &i.expr)
                || i.diverge
                    .as_ref()
                    .is_some_and(|(_, d)| contains_rec(ctx, d))
        }),
        Stmt::Expr(e, _) => contains_rec(ctx, e),
        Stmt::Item(_) => false,
        Stmt::Macro(m) => ctx.names().iter().any(|n| tokens_mention(&m.mac.tokens, n)),
    }
}

/// Bindings introduced by a pattern.
///
/// `syn` parses a unit-variant path such as `None` as `Pat::Ident`, so this
/// applies the usual convention and only treats lowercase-initial identifiers as
/// bindings. Threading a variant name as if it were a value would not compile.
pub(super) fn pat_bindings(pat: &Pat) -> Vec<Ident> {
    struct V(Vec<Ident>);
    impl<'ast> Visit<'ast> for V {
        fn visit_pat_ident(&mut self, p: &'ast PatIdent) {
            let s = p.ident.to_string();
            if s.starts_with(|c: char| c.is_lowercase() || c == '_') {
                self.0.push(p.ident.clone());
            }
            syn::visit::visit_pat_ident(self, p);
        }
    }
    let mut v = V(Vec::new());
    v.visit_pat(pat);
    v.0
}

/// Reject a payload parameter whose type is generic in the member that declares it, where another
/// member of the cycle calls that member without declaring the same parameter.
///
/// A group shares one driver, so a generic parameter has one instantiation for the whole group: the
/// driver's caller picks it, and the body must hold for every choice. A call from a member that
/// does not carry that parameter can only be passing some concrete type, which is the one thing the
/// rigid parameter cannot be. Left to rustc it is an `E0308` between the payload slot and the
/// argument, reported against the attribute.
pub(super) fn reject_generic_payload(ctx: &Ctx, funcs: &[ItemFn]) -> syn::Result<()> {
    fn type_params(func: &ItemFn) -> Vec<Ident> {
        func.sig
            .generics
            .params
            .iter()
            .filter_map(|p| match p {
                syn::GenericParam::Type(t) => Some(t.ident.clone()),
                _ => None,
            })
            .collect()
    }

    fn mentions(ty: &syn::Type, name: &Ident) -> bool {
        struct V<'a> {
            name: &'a Ident,
            found: bool,
        }
        impl<'ast> Visit<'ast> for V<'_> {
            fn visit_path(&mut self, path: &'ast syn::Path) {
                if path.is_ident(self.name) {
                    self.found = true;
                }
                syn::visit::visit_path(self, path);
            }
        }
        let mut v = V { name, found: false };
        v.visit_type(ty);
        v.found
    }

    /// Which members each member calls.
    fn callees(ctx: &Ctx, block: &Block) -> Vec<usize> {
        struct V<'a> {
            ctx: &'a Ctx,
            out: Vec<usize>,
        }
        impl<'ast> Visit<'ast> for V<'_> {
            fn visit_expr(&mut self, e: &'ast Expr) {
                if let Some((callee, _)) = self.ctx.rec_call(e) {
                    self.out.push(callee);
                }
                syn::visit::visit_expr(self, e);
            }
        }
        let mut v = V {
            ctx,
            out: Vec::new(),
        };
        v.visit_block(block);
        v.out
    }

    for (i, callee) in funcs.iter().enumerate() {
        let params = type_params(callee);
        if params.is_empty() {
            continue;
        }
        for arg in &callee.sig.inputs {
            let syn::FnArg::Typed(pt) = arg else { continue };
            // A `&mut` parameter is lent by the driver rather than carried, so it is not a slot.
            if matches!(&*pt.ty, syn::Type::Reference(r) if r.mutability.is_some()) {
                continue;
            }
            let Some(generic) = params.iter().find(|g| mentions(&pt.ty, g)) else {
                continue;
            };
            for (c, caller) in funcs.iter().enumerate() {
                if c == i || !callees(ctx, &caller.block).contains(&i) {
                    continue;
                }
                if type_params(caller).iter().any(|g| g == generic) {
                    continue;
                }
                // `desugar_apit` names an `impl Trait` parameter for its own use; the user
                // never wrote that name, so describe the parameter as they spelled it.
                let what = match generic.to_string().starts_with("__SsApit") {
                    true => "is an `impl Trait` parameter".to_string(),
                    false => format!("is generic in `{generic}`"),
                };
                return Err(syn::Error::new(
                    pt.span(),
                    format!(
                        "this parameter of `{}` {what}, and `{}` calls `{}` without that \
                         parameter. A cycle shares one driver, so the parameter has a single \
                         instantiation for the whole group and cannot also be whatever `{}` \
                         passes: give it a concrete type",
                        callee.sig.ident, caller.sig.ident, callee.sig.ident, caller.sig.ident,
                    ),
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Parameter normalisation
// ---------------------------------------------------------------------------

/// Give every payload parameter that destructures a name of its own, and re-bind the pattern
/// at the top of the body.
///
/// The expansion has to rebuild the argument tuple as an *expression*, which a pattern cannot
/// be, so a parameter has to have a name. That is a mechanical rewrite rather than a reason to
/// refuse: `f((a, b): (u64, u64))` becomes `f(__ss_arg0: (u64, u64))` with
/// `let (a, b): (u64, u64) = __ss_arg0;` prepended, which is what the rejection this replaces
/// used to ask the caller to write by hand. The body then sees exactly the bindings it wrote,
/// and the type is repeated on the `let` so that nothing about it is left to inference.
///
/// A `&mut` parameter is the one that cannot be treated this way: it is not a value the body
/// holds but a context slot the driver lends out and every step re-derives, so there is
/// nothing here to take apart. It keeps a rejection of its own.
pub(super) fn desugar_param_patterns(func: &mut ItemFn) -> syn::Result<()> {
    let mut lets: Vec<Stmt> = Vec::new();
    for (i, arg) in func.sig.inputs.iter_mut().enumerate() {
        let syn::FnArg::Typed(pt) = arg else { continue };
        // A plain `x` or `mut x` is already a name; everything else is a pattern.
        if matches!(
            &*pt.pat,
            Pat::Ident(PatIdent {
                by_ref: None,
                subpat: None,
                ..
            })
        ) {
            continue;
        }
        if is_context_slot(&pt.ty) {
            return Err(syn::Error::new(
                pt.pat.span(),
                "`#[stack_safe]` cannot destructure a `&mut` parameter: that parameter is not a \
                 value the body holds but a context slot the driver lends out, which every step \
                 re-derives, so there is nothing here to take apart. Take the parameter as a \
                 plain identifier and destructure what it points at inside the body",
            ));
        }
        let name = format_ident!("__ss_arg{}", i);
        let (pat, ty) = (pt.pat.clone(), pt.ty.clone());
        lets.push(parse_quote! { let #pat: #ty = #name; });
        *pt.pat = parse_quote! { #name };
    }
    // Prepended in order, so the bindings arrive in the order the parameters were written.
    for stmt in lets.into_iter().rev() {
        func.block.stmts.insert(0, stmt);
    }
    Ok(())
}

/// Does this body assign to `name` itself, rather than through it?
///
/// Asked of a `mut` binding on a `&mut` parameter, which becomes a context slot every step
/// re-derives: reassigning *the binding* would not be visible to the next step, while writing
/// *through* it (`*out = ..`, `out.push(..)`) is the ordinary use and fine. Only the first is
/// looked for, and only in this body: an assignment inside a nested item is that item's own.
///
/// A local of the same name shadowing the parameter is counted too, which errs towards the
/// rejection — the message says what to do either way.
pub(super) fn assigns_binding(block: &Block, name: &Ident) -> bool {
    struct V<'a> {
        name: &'a Ident,
        found: bool,
    }

    impl V<'_> {
        /// Is this the bare binding, as opposed to a place reached through it?
        fn is_binding(&self, e: &Expr) -> bool {
            matches!(strip_parens(e), Expr::Path(p)
                if p.qself.is_none()
                    && p.path.segments.len() == 1
                    && &p.path.segments[0].ident == self.name)
        }
    }

    impl<'ast> Visit<'ast> for V<'_> {
        fn visit_expr_assign(&mut self, a: &'ast syn::ExprAssign) {
            if self.is_binding(&a.left) {
                self.found = true;
            }
            syn::visit::visit_expr_assign(self, a);
        }

        // A compound assignment is a `Expr::Binary` with an assigning operator.
        fn visit_expr_binary(&mut self, b: &'ast syn::ExprBinary) {
            let assigns = matches!(
                b.op,
                syn::BinOp::AddAssign(_)
                    | syn::BinOp::SubAssign(_)
                    | syn::BinOp::MulAssign(_)
                    | syn::BinOp::DivAssign(_)
                    | syn::BinOp::RemAssign(_)
                    | syn::BinOp::BitXorAssign(_)
                    | syn::BinOp::BitAndAssign(_)
                    | syn::BinOp::BitOrAssign(_)
                    | syn::BinOp::ShlAssign(_)
                    | syn::BinOp::ShrAssign(_)
            );
            if assigns && self.is_binding(&b.left) {
                self.found = true;
            }
            syn::visit::visit_expr_binary(self, b);
        }

        fn visit_item(&mut self, _: &'ast syn::Item) {}
    }

    let mut v = V { name, found: false };
    v.visit_block(block);
    v.found
}
