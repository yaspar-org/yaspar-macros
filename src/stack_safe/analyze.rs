// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Pre-generation passes: normalise methods and parameters, mark pinned payloads and raw
//! context slots, and reject what the transform cannot rewrite.

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

/// A method split into a forwarding wrapper and a receiver-free function.
pub(super) struct MethodSplit {
    /// The original method, now forwarding to `inner`.
    pub(super) outer: ItemFn,
    /// Name of the associated function holding the transformed body.
    pub(super) inner: Ident,
}

/// Turn each `impl Trait` parameter into a named generic with the same bounds, so its type can be
/// written down.
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

/// Turn a `&self` / `&mut self` method into an associated function taking `__ss_self`,
/// returning the wrapper that forwards to it (`None` for a non-method). Also runs
/// `rewrite_self_calls`.
///
/// ```text
/// fn len(&self) -> usize { .. tail.len() .. }
///   ->  fn len(&self) -> usize { Self::__ss_impl_len(self) }
///       fn __ss_impl_len(__ss_self: &Self) -> usize { .. len(&tail) .. }
/// ```
///
/// The inner function is associated, not nested, because a nested `fn` cannot name `Self`.
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

    // The wrapper forwards by name.
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

    let mut outer = ItemFn {
        attrs: std::mem::take(&mut func.attrs),
        vis: func.vis.clone(),
        modifiers: func.modifiers.clone(),
        sig: func.sig.clone(),
        block: parse_quote! { { Self::#inner(self #(, #forwarded)*) } },
    };
    outer.attrs.push(parse_quote! { #[inline] });
    // `#[track_caller]` must cover every frame down to the body.
    if outer
        .attrs
        .iter()
        .any(|a| a.path().is_ident("track_caller"))
    {
        func.attrs.push(parse_quote! { #[track_caller] });
    }
    func.vis = syn::Visibility::Inherited;

    let rest: Vec<syn::FnArg> = func.sig.inputs.iter().skip(1).cloned().collect();
    func.sig.inputs = parse_quote! { #receiver: #receiver_ty #(, #rest)* };

    Ok(Some(MethodSplit { outer, inner }))
}

/// Rewrite group calls to plain-call form (`x.f(..)` -> `f(&x, ..)`, `Self::f(..)` -> `f(..)`)
/// and rename `self` to `__ss_self`. `self` itself is passed unchanged, so a `&mut` receiver
/// stays the same context slot rather than a derived one.
fn rewrite_self_calls(func: &mut ItemFn, group: &[Ident], receiver_is_mut: bool) {
    struct V<'a> {
        /// Every member of the group.
        group: &'a [Ident],
        receiver_is_mut: bool,
    }

    fn is_self(e: &Expr) -> bool {
        matches!(e, Expr::Path(p)
            if p.qself.is_none() && p.path.segments.len() == 1 && p.path.segments[0].ident == "self")
    }

    impl VisitMut for V<'_> {
        // Top-down, so a call is matched before its `self` receiver is renamed.
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

/// Rename calls (`g(..)`, `self::g(..)`, `Self::g(..)`, `x.g(..)`) per `renames`, including in
/// nested items. Used to keep a checked-only copy of the original calling other originals.
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

/// A reference argument whose target must outlive the call (which becomes a `return`) and so
/// goes into the driver's store.
pub(super) enum Lend<'e> {
    /// `&Node::Cons(..)`, `&t.child(i)`, `&local`: the value moves into the store.
    Whole(&'e Expr),
    /// `&local.f`, `&local[i]`: `root` is parked and the pointer targets `place` inside it.
    Place { root: Ident, place: &'e Expr },
}

/// The value a call lends the callee, if it lends one.
pub(super) fn borrows_a_built_value<'e>(
    ctx: &Ctx,
    member: usize,
    arg: &'e Expr,
) -> Option<Lend<'e>> {
    let Expr::Reference(r) = strip_parens(arg) else {
        return None;
    };
    let inner = strip_parens(&r.expr);
    let whole = matches!(
        inner,
        Expr::Call(_) | Expr::MethodCall(_) | Expr::Struct(_) | Expr::Macro(_)
    ) || ctx.owns_named_local(member, inner);
    if whole {
        return Some(Lend::Whole(&r.expr));
    }
    lent_place_root(ctx, member, arg).map(|(place, root)| Lend::Place {
        root: root.clone(),
        place,
    })
}

/// For `&<local>.f`, `&<local>[i]`, `&*<local>` and the like, where `<local>` is owned by this
/// member: the borrowed place and its root local.
fn lent_place_root<'e>(ctx: &Ctx, member: usize, arg: &'e Expr) -> Option<(&'e Expr, &'e Ident)> {
    let Expr::Reference(r) = strip_parens(arg) else {
        return None;
    };
    let mut place = strip_parens(&r.expr);
    let mut projected = false;
    loop {
        match place {
            Expr::Field(f) => {
                projected = true;
                place = strip_parens(&f.base);
            }
            Expr::Index(i) => {
                projected = true;
                place = strip_parens(&i.expr);
            }
            Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => {
                projected = true;
                place = strip_parens(&u.expr);
            }
            Expr::Path(p) => {
                let name = p.path.get_ident()?;
                let owned = ctx.owns_annotated_local(member, place);
                return (projected && owned).then_some((&*r.expr, name));
            }
            _ => return None,
        }
    }
}

/// Mark payload positions that some call passes a [`Lend`] to, so `emit` moves the value into
/// the pinned store. Without `data_in_frame` this is an error.
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

/// Check context-slot arguments of recursive calls. A derived reference such as
/// `walk(&mut t.kids[i])` marks the slot raw (a pointer parked for the child's subtree); it
/// requires `use_nonlinear_mut`.
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
                // Wrong arity was already reported by `validate`.
                if call.args.len() == callee.arity {
                    for (i, arg) in call.args.iter().enumerate() {
                        let Some(&slot) = callee.context_at.get(&i) else {
                            continue;
                        };
                        let entry = &self.ctx.context[slot];
                        match classify_ctx_arg(arg, &self.ctx.context) {
                            Some(CtxArg::Same) => {}
                            Some(CtxArg::Derived(place)) => {
                                // The place is spliced verbatim, so a call inside
                                // it would recurse natively.
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

/// Whether `name` appears in `body` after block `at` (not inside it), in source order.
fn read_after_block(body: &Block, at: &Block, name: &Ident) -> bool {
    struct V<'a> {
        at: *const Block,
        name: &'a Ident,
        passed: bool,
        found: bool,
    }

    impl<'ast> Visit<'ast> for V<'_> {
        fn visit_block(&mut self, b: &'ast Block) {
            if std::ptr::eq(b, self.at) {
                self.passed = true;
                return;
            }
            syn::visit::visit_block(self, b);
        }

        fn visit_ident(&mut self, i: &'ast Ident) {
            if self.passed && i == self.name {
                self.found = true;
            }
        }
    }

    let mut v = V {
        at: std::ptr::from_ref(at),
        name,
        passed: false,
        found: false,
    };
    v.visit_block(body);
    v.found
}

/// Reject a `let` in a nested, recursing block that shadows an outer binding read after that
/// block. Payload slots are keyed by name, so the resumed code would read the inner value.
pub(super) fn reject_shadowed_across_a_call(ctx: &Ctx, func: &ItemFn) -> syn::Result<()> {
    struct W<'a> {
        ctx: &'a Ctx,
        /// Bindings per scope: parameters, body, then nested blocks.
        scopes: Vec<Vec<Ident>>,
        body: &'a Block,
        err: Option<syn::Error>,
    }

    impl W<'_> {
        fn check(&mut self, pat: &Pat, recurses: bool, block: &Block) {
            if !recurses || self.scopes.len() <= 2 || self.err.is_some() {
                return;
            }
            for name in pat_bindings(pat) {
                let outer = self.scopes[..self.scopes.len() - 1]
                    .iter()
                    .any(|s| s.iter().any(|n| n == &name));
                if outer && read_after_block(self.body, block, &name) {
                    self.err = Some(syn::Error::new(
                        name.span(),
                        format!(
                            "`{name}` shadows a binding of the same name outside this block, the \
                             block recurses, and the outer one is read again afterwards. \
                             `#[stack_safe]` parks the locals that are live across a recursive call \
                             by *name*, so the two are one slot: the code that resumes would read \
                             this binding where the source had gone back to the outer one. Rename \
                             one of them",
                        ),
                    ));
                    return;
                }
            }
        }

        fn bind(&mut self, pat: &Pat) {
            if let Some(scope) = self.scopes.last_mut() {
                scope.extend(pat_bindings(pat));
            }
        }

        fn block(&mut self, b: &Block) {
            let recurses = b.stmts.iter().any(|s| stmt_contains_rec(self.ctx, s));
            self.scopes.push(Vec::new());
            for stmt in &b.stmts {
                if let Stmt::Local(l) = stmt {
                    self.check(&l.pat, recurses, b);
                    self.bind(&l.pat);
                }
                self.walk_stmt(stmt);
            }
            self.scopes.pop();
        }

        /// Recurse into blocks, scoping `for`, `match` arm, and closure bindings.
        fn walk_stmt(&mut self, stmt: &Stmt) {
            struct V<'a, 'b>(&'a mut W<'b>);

            impl<'ast> Visit<'ast> for V<'_, '_> {
                fn visit_block(&mut self, b: &'ast Block) {
                    self.0.block(b);
                }

                fn visit_arm(&mut self, a: &'ast syn::Arm) {
                    self.0.scopes.push(Vec::new());
                    self.0.bind(&a.pat);
                    syn::visit::visit_arm(self, a);
                    self.0.scopes.pop();
                }

                fn visit_expr_for_loop(&mut self, f: &'ast syn::ExprForLoop) {
                    self.visit_expr(&f.expr);
                    self.0.scopes.push(Vec::new());
                    self.0.bind(&f.pat);
                    self.0.block(&f.body);
                    self.0.scopes.pop();
                }

                fn visit_expr_closure(&mut self, c: &'ast syn::ExprClosure) {
                    self.0.scopes.push(Vec::new());
                    for input in &c.inputs {
                        self.0.bind(input);
                    }
                    self.visit_expr(&c.body);
                    self.0.scopes.pop();
                }

                fn visit_item(&mut self, _: &'ast syn::Item) {}
            }

            V(self).visit_stmt(stmt);
        }
    }

    let params = func
        .sig
        .inputs
        .iter()
        .filter_map(|a| match a {
            syn::FnArg::Typed(pt) => match &*pt.pat {
                Pat::Ident(p) => Some(p.ident.clone()),
                _ => None,
            },
            syn::FnArg::Receiver(_) => None,
        })
        .collect();
    let mut w = W {
        ctx,
        body: &func.block,
        scopes: vec![params],
        err: None,
    };
    w.block(&func.block);
    match w.err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Reject calls and bindings the transform would silently get wrong.
pub(super) fn validate(ctx: &Ctx, func: &ItemFn) -> syn::Result<()> {
    struct V<'a> {
        ctx: &'a Ctx,
        /// Type and const generics of the enclosing function.
        own_generics: Vec<String>,
        err: Option<syn::Error>,
    }

    impl V<'_> {
        fn fail(&mut self, span: Span, msg: &str) {
            if self.err.is_none() {
                self.err = Some(syn::Error::new(span, msg));
            }
        }

        /// Reject a turbofish other than the function's own parameters: every recursive call
        /// re-enters the same instantiation.
        fn check_generic_args(&mut self, call: &syn::ExprCall) {
            let Expr::Path(p) = &*call.func else { return };
            let Some(seg) = p.path.segments.last() else {
                return;
            };
            let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
                return;
            };
            let restates_own = args.args.iter().all(|a| match a {
                syn::GenericArgument::Lifetime(_) => true,
                syn::GenericArgument::Type(syn::Type::Path(t)) => t
                    .path
                    .get_ident()
                    .is_some_and(|id| self.own_generics.iter().any(|g| g == &id.to_string())),
                syn::GenericArgument::Const(Expr::Path(c)) => c
                    .path
                    .get_ident()
                    .is_some_and(|id| self.own_generics.iter().any(|g| g == &id.to_string())),
                _ => false,
            });
            if !restates_own {
                self.fail(
                    args.span(),
                    &format!(
                        "`#[stack_safe]` does not support explicit generic arguments on a \
                         recursive call: the rewritten body is one loop, compiled for the \
                         instantiation it was entered at, and a recursive call re-enters that same \
                         loop. `{}::<..>` with arguments of its own is a call to a *different* \
                         function, which no transition can reach — it would silently run the \
                         caller's instantiation. Give the enclosing function's own parameters, or \
                         move the differently instantiated call into a function of its own and \
                         call that",
                        seg.ident,
                    ),
                );
            }
        }

        /// Reject a binding named like a member: calls are matched by name.
        fn check_shadowing(&mut self, pat: &Pat) {
            for bound in pat_bindings(pat) {
                if self.ctx.index_of(&bound).is_some() {
                    self.fail(
                        bound.span(),
                        &format!(
                            "`{bound}` is the name of a function `#[stack_safe]` is rewriting, and \
                             this binding shadows it. Calls are recognised by name, since a macro \
                             resolves no paths, so a call to this binding would be rewritten into a \
                             recursion instead. Rename the binding",
                        ),
                    );
                    return;
                }
            }
        }

        /// Reject an item declared in a block before a recursive call: the continuation is a
        /// separate `match` arm, where the item is out of scope.
        fn check_block_items(&mut self, block: &Block) {
            let mut seen: Option<Span> = None;
            for stmt in &block.stmts {
                if let Stmt::Item(item) = stmt {
                    // Members are moved into the driver; `Verbatim` and macro items are exempt.
                    let ours = match item {
                        syn::Item::Fn(f) => self.ctx.index_of(&f.sig.ident).is_some(),
                        syn::Item::Verbatim(_) | syn::Item::Macro(_) => true,
                        _ => false,
                    };
                    if !ours {
                        seen = Some(item.span());
                    }
                    continue;
                }
                if let Some(at) = seen
                    && stmt_contains_rec(self.ctx, stmt)
                {
                    self.fail(
                        at,
                        "`#[stack_safe]` cannot keep this item in scope: the block declares it and \
                         then recurses, and the code after a recursive call becomes a separate arm \
                         of one `match`, which carries values but not declarations — the name would \
                         resolve to whatever encloses the function instead. Move the item out to \
                         the function's own body, whose items are moved out with it",
                    );
                    return;
                }
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
                self.check_generic_args(call);
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
            if let Expr::Closure(c) = e {
                for input in &c.inputs {
                    self.check_shadowing(input);
                }
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

        // Statement-position macros are `Stmt::Macro`, not `Expr::Macro`.
        fn visit_stmt_macro(&mut self, m: &'ast syn::StmtMacro) {
            self.check_macro(&m.mac, m.span());
        }

        fn visit_local(&mut self, l: &'ast syn::Local) {
            self.check_shadowing(&l.pat);
            syn::visit::visit_local(self, l);
        }

        fn visit_arm(&mut self, a: &'ast syn::Arm) {
            self.check_shadowing(&a.pat);
            syn::visit::visit_arm(self, a);
        }

        fn visit_block(&mut self, b: &'ast Block) {
            self.check_block_items(b);
            syn::visit::visit_block(self, b);
        }

        fn visit_item(&mut self, _: &'ast syn::Item) {}
    }

    let own_generics = func
        .sig
        .generics
        .params
        .iter()
        .filter_map(|p| match p {
            syn::GenericParam::Type(t) => Some(t.ident.to_string()),
            syn::GenericParam::Const(c) => Some(c.ident.to_string()),
            syn::GenericParam::Lifetime(_) => None,
        })
        .collect();
    let mut v = V {
        ctx,
        own_generics,
        err: None,
    };
    // Skip the body's own block: its top-level items are moved out to enclose every arm.
    for stmt in &func.block.stmts {
        v.visit_stmt(stmt);
    }
    match v.err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Whether `name` appears anywhere in `ts`.
pub(super) fn tokens_mention(ts: &TokenStream, name: &Ident) -> bool {
    ts.clone().into_iter().any(|t| match t {
        TokenTree::Ident(i) => i == *name,
        TokenTree::Group(g) => tokens_mention(&g.stream(), name),
        _ => false,
    })
}

/// Whether `e` contains a recursive call, outside nested items.
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

/// Whether `s` contains a recursive call; a macro counts if it mentions a member.
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

/// Bindings introduced by a pattern. Only identifiers starting lowercase or `_` count, since
/// `syn` parses unit variants like `None` as `Pat::Ident`.
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

/// Reject a generic payload parameter when a group member lacking that generic calls its owner:
/// the shared driver has one instantiation, so the caller can't pass a concrete type.
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

    /// Members called anywhere in `block`.
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
            // `&mut` parameters are context, not payload.
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
                // Don't show the name `desugar_apit` invented.
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

/// Replace each destructuring parameter with `__ss_argN` and prepend `let <pat>: <ty> = __ss_argN;`.
/// Rejects destructuring a `&mut` parameter.
pub(super) fn desugar_param_patterns(func: &mut ItemFn) -> syn::Result<()> {
    let mut lets: Vec<Stmt> = Vec::new();
    for (i, arg) in func.sig.inputs.iter_mut().enumerate() {
        let syn::FnArg::Typed(pt) = arg else { continue };
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
    for stmt in lets.into_iter().rev() {
        func.block.stmts.insert(0, stmt);
    }
    Ok(())
}

/// Whether `block` reassigns `name` itself (`name = ..`, `name += ..`), not through it. Nested
/// items are skipped; a shadowing local counts.
pub(super) fn assigns_binding(block: &Block, name: &Ident) -> bool {
    struct V<'a> {
        name: &'a Ident,
        found: bool,
    }

    impl V<'_> {
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
