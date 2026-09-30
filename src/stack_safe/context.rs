// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Context parameters: references the driver owns and reborrows for each step instead of
//! carrying them in the payload. Needed for `&mut` (two frames holding one `&mut` is E0505);
//! `emit` also promotes shared references that every recursive call passes unchanged.

use proc_macro2::{Ident, TokenStream};
use quote::quote;
use std::cell::Cell;
use syn::Expr;

use super::names::ctx_param;

/// One parameter threaded through the driver rather than the payload.
pub(super) struct CtxEntry {
    /// Name in the body (`__ss_self` for a receiver).
    pub(super) name: Ident,
    /// `&mut T` rather than `&T`.
    pub(super) mutable: bool,
    /// Initial value at the outer call (`out`, `self`).
    pub(super) init: TokenStream,
    /// Declared reference type (`&mut Tree`, `&Self`). Spelled out in [`Self::rebind`] because
    /// inference can fail on `&mut *ptr` in a diverging arm.
    pub(super) ty: TokenStream,
    /// The slot holds a raw pointer, because a call passes a derived place here
    /// (`use_nonlinear_mut`). Set by `analyze::scan_context_args`.
    pub(super) raw: Cell<bool>,
}

impl CtxEntry {
    /// Initial value, converted to a pointer for a raw slot.
    pub(super) fn init_expr(&self) -> TokenStream {
        let init = &self.init;
        match (self.raw.get(), self.mutable) {
            (false, _) => init.clone(),
            (true, true) => quote! { ::core::ptr::from_mut(#init) },
            (true, false) => quote! { ::core::ptr::from_ref(#init) },
        }
    }

    /// `let name: T = <reborrow of slot i>;`, emitted at the top of every arm so no frame carries
    /// a reborrow.
    pub(super) fn rebind(&self, i: usize) -> TokenStream {
        let (name, ctx, idx) = (&self.name, ctx_param(), syn::Index::from(i));
        let ty = &self.ty;
        match (self.raw.get(), self.mutable) {
            // SAFETY: a raw slot holds either the outer call's reference or a pointer derived
            // from it (checked by `analyze::scan_context_args`), and the parent's pointer is
            // restored before its frame resumes. So it always points into a borrow the outermost
            // call holds, and only one reborrow is live at a time. Tested under Miri in
            // `tests/context.rs`.
            (true, true) => quote! { let #name: #ty = unsafe { &mut *#ctx.#idx }; },
            (true, false) => quote! { let #name: #ty = unsafe { &*#ctx.#idx }; },
            (false, true) => quote! { let #name: #ty = &mut *#ctx.#idx; },
            (false, false) => quote! { let #name: #ty = &*#ctx.#idx; },
        }
    }
}

/// What a recursive call passes for a context position.
pub(super) enum CtxArg {
    /// The binding itself (`out`, `&mut *out`): the child shares the slot.
    Same,
    /// A place rooted at the binding (`&mut t.kids[i]`): swapped in for the child, then restored.
    Derived(Expr),
}

/// Classify an argument at a context position; `None` if unsupported.
pub(super) fn classify_ctx_arg(arg: &Expr, entries: &[CtxEntry]) -> Option<CtxArg> {
    let is_ctx_name = |id: &Ident| entries.iter().any(|e| &e.name == id);
    match strip_parens(arg) {
        Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
            is_ctx_name(&p.path.segments[0].ident).then_some(CtxArg::Same)
        }
        Expr::Reference(r) => {
            // `&mut *out` is the same slot; otherwise it must be a place rooted at a context
            // binding.
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

/// Strip parentheses and invisible groups from a type.
pub(super) fn peel_type(ty: &syn::Type) -> &syn::Type {
    match ty {
        syn::Type::Paren(p) => peel_type(&p.elem),
        syn::Type::Group(g) => peel_type(&g.elem),
        other => other,
    }
}

/// Whether a parameter type is a syntactic `&mut` and so always a context slot. A `&mut` hidden
/// behind a type alias is not detected.
pub(super) fn is_context_slot(ty: &syn::Type) -> bool {
    matches!(peel_type(ty), syn::Type::Reference(r) if r.mutability.is_some())
}

/// A slot's type as the driver spells it: parentheses peeled and named lifetimes other than
/// `'static` erased, so one spelling works for the whole group.
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

/// Key for comparing members' slots: [`slot_type`] with `Self` replaced by `self_ty`.
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

/// The local a place expression is rooted at, if it is a place.
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
