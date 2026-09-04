// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Context parameters: the `&mut` parameters and receiver that the driver owns
//! and lends out, instead of letting them travel in the argument payload.

use proc_macro2::{Ident, TokenStream};
use quote::quote;
use std::cell::Cell;
use syn::Expr;

use super::names::ctx_param;

// Context parameters
//
// A `&mut` parameter cannot travel in the argument payload: the payload is moved
// into a continuation, so two live frames would hold the same `&mut` and
// borrowck rejects it (E0505). Such a parameter instead becomes part of a
// *context* tuple that the driver owns and lends out — one reborrow per body
// invocation and per continuation resume, so nothing captures it.
//
// Shared (`&`) parameters need none of this: they are `Copy`, so the payload is
// fine, and they stay there.
// ---------------------------------------------------------------------------

/// One parameter threaded through the driver rather than the payload.
pub(super) struct CtxEntry {
    /// What the body calls it. `__ss_self` for a receiver.
    pub(super) name: Ident,
    /// `&mut T` rather than `&T`.
    pub(super) mutable: bool,
    /// The expression that fills this slot at the outer call site (`out`, `self`).
    pub(super) init: TokenStream,
    /// The slot's declared reference type, `&mut Tree` or `&Self`. The rebinding
    /// names it rather than leaving it to inference: a raw slot is reached through
    /// `&mut *ptr`, whose pointee inference can fail to resolve in an arm that
    /// diverges — and then the user sees `type annotations needed` pointing at their
    /// own code with no way to act on it.
    pub(super) ty: TokenStream,
    /// Some recursive call passes a reference *derived* from a context parameter
    /// here, so the slot holds a raw pointer: the derived pointer is swapped in
    /// for the child subtree and the parent's is restored by its continuation.
    /// Only reachable with `use_nonlinear_mut`; set by `scan_context_args`.
    pub(super) raw: Cell<bool>,
}

impl CtxEntry {
    /// The expression that fills this slot, wrapped for a raw slot: that slot
    /// holds a pointer, so it is initialised *from* the reference rather than
    /// storing the reference itself.
    pub(super) fn init_expr(&self) -> TokenStream {
        let init = &self.init;
        match (self.raw.get(), self.mutable) {
            (false, _) => init.clone(),
            (true, true) => quote! { ::core::ptr::from_mut(#init) },
            (true, false) => quote! { ::core::ptr::from_ref(#init) },
        }
    }

    /// `let name: &mut T = <reborrow of slot i>;` — emitted at the top of every arm,
    /// so that no reborrow is ever carried in a frame.
    pub(super) fn rebind(&self, i: usize) -> TokenStream {
        let (name, ctx, idx) = (&self.name, ctx_param(), syn::Index::from(i));
        let ty = &self.ty;
        match (self.raw.get(), self.mutable) {
            // The deref of a raw pointer detaches the borrow, which is what lets
            // a swap assign to the slot while this reborrow is still live.
            //
            // SAFETY: emitted into the caller's crate, so the invariant is ours. A raw
            // slot is written only by `init_expr`, from a reference the original was
            // already given, and by a recursive call swapping in a pointer derived from
            // that same reference — which `analyze::scan_context_args` has checked is
            // rooted at this context parameter. The continuation restores the parent's
            // pointer before the parent's frame resumes, so the slot always names a
            // place inside a borrow the outermost call still holds, and only one
            // reborrow is live at a time, since no frame carries one. Gated behind
            // `use_nonlinear_mut`; `tests/context.rs` covers it under both of Miri's
            // aliasing models.
            (true, true) => quote! { let #name: #ty = unsafe { &mut *#ctx.#idx }; },
            (true, false) => quote! { let #name: #ty = unsafe { &*#ctx.#idx }; },
            (false, true) => quote! { let #name: #ty = &mut *#ctx.#idx; },
            (false, false) => quote! { let #name: #ty = &*#ctx.#idx; },
        }
    }
}

/// What a recursive call passes for a context position.
pub(super) enum CtxArg {
    /// The context binding itself (`out`, `&mut *out`): the child shares the slot.
    Same,
    /// A place rooted at a context binding (`&mut t.kids[i]`): the slot has to be
    /// swapped for the child and restored afterwards.
    Derived(Expr),
}

/// Classify the argument at a context position, or `None` if the transform
/// cannot account for it.
pub(super) fn classify_ctx_arg(arg: &Expr, entries: &[CtxEntry]) -> Option<CtxArg> {
    let is_ctx_name = |id: &Ident| entries.iter().any(|e| &e.name == id);
    match strip_parens(arg) {
        Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
            is_ctx_name(&p.path.segments[0].ident).then_some(CtxArg::Same)
        }
        Expr::Reference(r) => {
            // `&mut *out` is the same slot; anything else must be a place rooted
            // at a context binding, whose target therefore outlives the call and
            // does not move when frames move.
            if let Expr::Unary(u) = strip_parens(&r.expr)
                && matches!(u.op, syn::UnOp::Deref(_))
                && let Expr::Path(p) = strip_parens(&u.expr)
                && p.qself.is_none()
                && p.path.segments.len() == 1
                && is_ctx_name(&p.path.segments[0].ident)
            {
                return Some(CtxArg::Same);
            }
            place_root(&r.expr)
                .filter(|root| is_ctx_name(root))
                .map(|_| CtxArg::Derived(arg.clone()))
        }
        _ => None,
    }
}

/// A type with its parentheses and invisible groups stripped.
///
/// `(&mut Vec<u64>)` is the `&mut` parameter it plainly is: parenthesising a type changes
/// nothing about it, and a parameter that slipped past the test below travelled in the
/// payload instead of becoming a slot — an `E0505` blamed on the attribute.
pub(super) fn peel_type(ty: &syn::Type) -> &syn::Type {
    match ty {
        syn::Type::Paren(p) => peel_type(&p.elem),
        syn::Type::Group(g) => peel_type(&g.elem),
        other => other,
    }
}

/// Does a parameter of this type become a context slot rather than payload?
///
/// Only a `&mut` can: a shared reference is `Copy`, so the payload is fine for it, and an
/// owned value moves into the payload like any other. Asked of the peeled type, so that
/// however the reference is parenthesised it is the same answer.
///
/// One shape cannot be seen from the tokens and so is missed: a `&mut` hidden behind a type
/// alias, `type Out<'a> = &'a mut Vec<u64>`. Treating every unrecognised type as a slot would
/// be far worse — every by-value parameter is one — so an alias has to be written out.
pub(super) fn is_context_slot(ty: &syn::Type) -> bool {
    matches!(peel_type(ty), syn::Type::Reference(r) if r.mutability.is_some())
}

/// A slot's type as the driver names it: parentheses peeled, and every lifetime the user
/// named erased.
///
/// The rebinding this annotates is a `let` in the driver, where a lifetime is inferred, and
/// the group's members are free to have named theirs differently — or not at all — so the
/// one spelling kept for the whole group must not name a lifetime only one member declares.
/// `'static` is left alone: it is a requirement rather than a name, and a slot that asks for
/// it is not the same slot as one that does not.
pub(super) fn slot_type(ty: &syn::Type) -> syn::Type {
    struct V;

    impl syn::visit_mut::VisitMut for V {
        fn visit_type_mut(&mut self, ty: &mut syn::Type) {
            *ty = peel_type(ty).clone();
            syn::visit_mut::visit_type_mut(self, ty);
        }

        fn visit_type_reference_mut(&mut self, r: &mut syn::TypeReference) {
            if r.lifetime.as_ref().is_some_and(|l| l.ident != "static") {
                r.lifetime = None;
            }
            syn::visit_mut::visit_type_reference_mut(self, r);
        }

        fn visit_lifetime_mut(&mut self, l: &mut syn::Lifetime) {
            if l.ident != "static" {
                *l = syn::Lifetime::new("'_", l.apostrophe);
            }
        }
    }

    let mut ty = ty.clone();
    syn::visit_mut::VisitMut::visit_type_mut(&mut V, &mut ty);
    ty
}

/// What two members' slots are compared by: [`slot_type`], plus `Self` resolved to the type
/// the impl block is for, where the caller knows it.
///
/// Compared as types rather than as rendered text, so that two spellings of one type — `&'a
/// mut Vec<u64>` and `&mut Vec<u64>`, or `&mut Self` and `&mut S` inside one impl — are not
/// presented to the user as their own mistake.
pub(super) fn slot_key(ty: &syn::Type, self_ty: Option<&syn::Type>) -> syn::Type {
    struct V<'a> {
        self_ty: &'a syn::Type,
    }

    impl syn::visit_mut::VisitMut for V<'_> {
        fn visit_type_mut(&mut self, ty: &mut syn::Type) {
            if let syn::Type::Path(p) = &*ty
                && p.qself.is_none()
                && p.path.is_ident("Self")
            {
                *ty = self.self_ty.clone();
                return;
            }
            syn::visit_mut::visit_type_mut(self, ty);
        }
    }

    let mut ty = slot_type(ty);
    if let Some(self_ty) = self_ty {
        syn::visit_mut::VisitMut::visit_type_mut(&mut V { self_ty }, &mut ty);
    }
    ty
}

pub(super) fn strip_parens(e: &Expr) -> &Expr {
    match e {
        Expr::Paren(p) => strip_parens(&p.expr),
        Expr::Group(g) => strip_parens(&g.expr),
        other => other,
    }
}

/// The identifier a place expression is rooted at, if it is a place at all.
pub(super) fn place_root(e: &Expr) -> Option<&Ident> {
    match strip_parens(e) {
        Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
            Some(&p.path.segments[0].ident)
        }
        Expr::Field(f) => place_root(&f.base),
        Expr::Index(i) => place_root(&i.expr),
        Expr::MethodCall(m) => place_root(&m.receiver),
        Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => place_root(&u.expr),
        _ => None,
    }
}
