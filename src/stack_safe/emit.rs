// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Emits one cycle's expansion: entry and frame enums, the driver, and a wrapper per member.
//! Finding cycles and placing drivers is `scan.rs`.

use proc_macro2::{Ident, TokenStream};
use quote::{ToTokens, format_ident, quote};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use syn::spanned::Spanned;
use syn::{FnArg, Item, ItemFn, Pat, PatIdent, ReturnType, Stmt};

use super::Opts;
use super::analyze::{
    MethodSplit, assigns_binding, desugar_apit, desugar_param_patterns, desugar_receiver,
    reject_generic_payload, reject_shadowed_across_a_call, scan_context_args, scan_pinned_args,
    validate,
};
use super::context::{CtxEntry, is_context_slot, peel_type, slot_key, slot_type, strip_parens};
use super::cps::cps_stmts;
use super::driver;
use super::loop_state::{Solved, solve_payloads, substitute};
use super::names::*;
use super::walk::{Ctx, Env, Member};

/// What one function contributes to its group's driver.
struct Split {
    context: Vec<CtxEntry>,
    member: Member,
    /// Normalized slot types, for comparing members. See `context::slot_key`.
    slot_keys: Vec<String>,
    /// Slot types as written, for error messages.
    slot_types: Vec<String>,
}

/// Split parameters into context slots (`&mut`, receivers, and invariant shared references) and
/// payload. Expects [`desugar_param_patterns`] and [`reject_unsupported_signature`] to have run.
fn split_params(
    func: &ItemFn,
    self_ty: Option<&syn::Type>,
    invariant_context: &HashSet<usize>,
) -> syn::Result<Split> {
    let sig = &func.sig;
    let func_body = func.block.clone();

    let mut param_pats = Vec::new();
    let mut param_anns: Vec<TokenStream> = Vec::new();
    let mut param_anns_pinned: Vec<TokenStream> = Vec::new();
    let mut param_pointees: Vec<Option<TokenStream>> = Vec::new();
    let mut param_types: Vec<TokenStream> = Vec::new();
    let mut param_bare_types: Vec<TokenStream> = Vec::new();
    let mut param_names = Vec::new();
    let mut context: Vec<CtxEntry> = Vec::new();
    let mut context_at: HashMap<usize, usize> = HashMap::new();
    let mut slot_keys: Vec<String> = Vec::new();
    let mut slot_types: Vec<String> = Vec::new();
    let mut arg_index = 0usize;
    for arg in &sig.inputs {
        match arg {
            // Unreachable: `desugar_receiver` already ran.
            FnArg::Receiver(r) => {
                return Err(syn::Error::new(
                    r.span(),
                    "`#[stack_safe]` could not rewrite this receiver into an ordinary parameter",
                ));
            }
            FnArg::Typed(pt) => {
                if let Some(attr) = pt
                    .attrs
                    .iter()
                    .find(|a| a.path().is_ident("cfg") || a.path().is_ident("cfg_attr"))
                {
                    return Err(syn::Error::new_spanned(
                        attr,
                        "`#[stack_safe]` cannot honour a `#[cfg]` on a parameter: the driver takes \
                         one payload shape for the whole cycle, and a parameter that comes and \
                         goes would change it. Write two gated definitions of the function, or \
                         gate the parameter's *value* inside the body",
                    ));
                }
                let Pat::Ident(PatIdent {
                    ident,
                    by_ref: None,
                    subpat: None,
                    mutability,
                    ..
                }) = &*pt.pat
                else {
                    // Unreachable: `desugar_param_patterns` names or rejects every other pattern.
                    return Err(syn::Error::new(
                        pt.pat.span(),
                        "`#[stack_safe]` requires plain identifier parameters; bind the pattern \
                         inside the body instead",
                    ));
                };
                if is_context_slot(&pt.ty) || invariant_context.contains(&arg_index) {
                    let mutable = matches!(
                        peel_type(&pt.ty),
                        syn::Type::Reference(r) if r.mutability.is_some()
                    );
                    // Reassigning a slot binding would be lost; each step re-derives it.
                    if let Some(m) = mutability
                        && assigns_binding(&func_body, ident)
                    {
                        return Err(syn::Error::new(
                            m.span(),
                            "`#[stack_safe]` does not support *assigning* to a `mut` binding of a \
                             `&mut` parameter: the parameter becomes a context slot that every \
                             step re-derives from the driver, so the new value would not be seen \
                             by the next step. Writing through the reference is fine; to walk a \
                             structure by reassigning, take the place as an ordinary parameter or \
                             use `#[stack_safe(use_nonlinear_mut)]`",
                        ));
                    }
                    context_at.insert(arg_index, context.len());
                    slot_keys.push(pretty_type(&slot_key(&pt.ty, self_ty)));
                    slot_types.push(pretty_type(&pt.ty));
                    // Lifetime-erased: the tuple type is shared by the whole group.
                    let ty = slot_type(&pt.ty);
                    let ty = &ty;
                    context.push(CtxEntry {
                        name: ident.clone(),
                        mutable,
                        init: quote! { #ident },
                        ty: quote! { #ty },
                        raw: Cell::new(false),
                    });
                } else {
                    param_pats.push(quote! { #mutability #ident });
                    param_names.push(ident.clone());
                    // See `annotatable`.
                    let ty = &pt.ty;
                    param_types.push(if !annotatable(ty) {
                        TokenStream::new()
                    } else {
                        quote! { : #ty }
                    });
                    param_bare_types.push(if !annotatable(ty) {
                        TokenStream::new()
                    } else {
                        quote! { #ty }
                    });
                    param_pointees.push(match &**ty {
                        syn::Type::Reference(r) => {
                            let elem = &r.elem;
                            Some(quote! { #elem })
                        }
                        _ => None,
                    });
                    param_anns.push(if !annotatable(ty) {
                        TokenStream::new()
                    } else {
                        quote! { let #mutability #ident: #ty = #ident; }
                    });
                    // Used when `scan_pinned_args` marks this position (`data_in_frame`): the
                    // payload is a pointer into the driver's pinned store. The pointer type is
                    // named first to fix the payload's type.
                    //
                    // SAFETY: the pointer came from `Pin::push`, which never moves its values,
                    // and the pushing frame drops it only after every arm that can see it.
                    param_anns_pinned.push(match &**ty {
                        syn::Type::Reference(r) => {
                            let elem = &r.elem;
                            quote! {
                                let #ident: *const #elem = #ident;
                                let #ident: #ty = unsafe { &*#ident };
                            }
                        }
                        _ => quote! { let #ident: #ty = unsafe { &*#ident }; },
                    });
                }
                arg_index += 1;
            }
        }
    }

    Ok(Split {
        context,
        member: Member {
            name: sig.ident.clone(),
            arity: arg_index,
            context_at,
            param_pats,
            pinned: param_names_len_cells(&param_names),
            param_pointees,
            param_names,
            param_anns,
            param_anns_pinned,
            param_types,
            param_bare_types,
        },
        slot_keys,
        slot_types,
    })
}

/// Per member, the positions of shared-reference parameters passed through unchanged by every
/// recursive call: same name and type in every member, never rebound, always passed as-is.
fn invariant_shared_contexts(
    funcs: &[ItemFn],
    assoc: bool,
    self_ty: Option<&syn::Type>,
) -> Vec<HashSet<usize>> {
    fn shared_param(
        func: &ItemFn,
        name: &str,
        key: &str,
        self_ty: Option<&syn::Type>,
    ) -> Option<usize> {
        func.sig.inputs.iter().enumerate().find_map(|(i, arg)| {
            let FnArg::Typed(pt) = arg else { return None };
            let Pat::Ident(PatIdent {
                ident,
                mutability: None,
                by_ref: None,
                subpat: None,
                ..
            }) = &*pt.pat
            else {
                return None;
            };
            let syn::Type::Reference(r) = peel_type(&pt.ty) else {
                return None;
            };
            (r.mutability.is_none()
                && ident == name
                && pretty_type(&slot_key(&pt.ty, self_ty)) == key)
                .then_some(i)
        })
    }

    fn body_binds(func: &ItemFn, name: &Ident) -> bool {
        struct V<'a> {
            name: &'a Ident,
            found: bool,
        }
        impl<'ast> syn::visit::Visit<'ast> for V<'_> {
            fn visit_pat_ident(&mut self, pat: &'ast PatIdent) {
                if &pat.ident == self.name {
                    self.found = true;
                    return;
                }
                syn::visit::visit_pat_ident(self, pat);
            }

            fn visit_item(&mut self, _: &'ast Item) {}
        }

        let mut v = V { name, found: false };
        syn::visit::Visit::visit_block(&mut v, &func.block);
        v.found
    }

    fn called_member(call: &syn::ExprCall, funcs: &[ItemFn], assoc: bool) -> Option<usize> {
        let syn::Expr::Path(p) = &*call.func else {
            return None;
        };
        let segments = &p.path.segments;
        let named = match segments.len() {
            1 => true,
            2 => !assoc && segments[0].ident == "self",
            _ => false,
        };
        if p.qself.is_some() || !named {
            return None;
        }
        let name = &segments.last()?.ident;
        funcs.iter().position(|f| &f.sig.ident == name)
    }

    let mut out = vec![HashSet::new(); funcs.len()];
    let Some(first) = funcs.first() else {
        return out;
    };

    for (first_pos, arg) in first.sig.inputs.iter().enumerate() {
        let FnArg::Typed(pt) = arg else { continue };
        let Pat::Ident(PatIdent {
            ident,
            mutability: None,
            by_ref: None,
            subpat: None,
            ..
        }) = &*pt.pat
        else {
            continue;
        };
        let syn::Type::Reference(r) = peel_type(&pt.ty) else {
            continue;
        };
        if r.mutability.is_some() {
            continue;
        }

        let key = pretty_type(&slot_key(&pt.ty, self_ty));
        let Some(positions) = funcs
            .iter()
            .map(|func| shared_param(func, &ident.to_string(), &key, self_ty))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        if positions[0] != first_pos
            || funcs
                .iter()
                .any(|func| body_binds(func, ident) || assigns_binding(&func.block, ident))
        {
            continue;
        }

        struct Calls<'a> {
            funcs: &'a [ItemFn],
            assoc: bool,
            positions: &'a [usize],
            name: &'a Ident,
            seen: bool,
            valid: bool,
        }
        impl<'ast> syn::visit::Visit<'ast> for Calls<'_> {
            fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
                if let Some(callee) = called_member(call, self.funcs, self.assoc) {
                    self.seen = true;
                    let same = call.args.get(self.positions[callee]).is_some_and(|arg| {
                        matches!(
                            strip_parens(arg),
                            syn::Expr::Path(p)
                                if p.qself.is_none()
                                    && p.path.segments.len() == 1
                                    && p.path.segments[0].ident == *self.name
                        )
                    });
                    self.valid &= same;
                }
                syn::visit::visit_expr_call(self, call);
            }

            fn visit_item(&mut self, _: &'ast Item) {}
        }

        let mut calls = Calls {
            funcs,
            assoc,
            positions: &positions,
            name: ident,
            seen: false,
            valid: true,
        };
        for func in funcs {
            syn::visit::Visit::visit_block(&mut calls, &func.block);
        }
        if calls.seen && calls.valid {
            for (slots, position) in out.iter_mut().zip(positions) {
                slots.insert(position);
            }
        }
    }
    out
}

/// Whether the group can share one machine, taking a seed enum of each member's parameters.
///
/// Not when: there is one member, a parameter is `impl Trait`, a parameter names `Self` with no
/// concrete type known, or [`shared_generics`] fails. An alias hiding a lifetime (`w: Words`)
/// passes this check but fails with `E0106`; `Words<'_>` fixes it.
fn liftable(funcs: &[ItemFn], has_self_ty: bool) -> bool {
    let writable = |ty: &syn::Type| -> bool {
        let (impl_trait, self_ty_named) = names_impl_trait_or_self(ty);
        !impl_trait && (has_self_ty || !self_ty_named)
    };

    funcs.len() > 1
        && shared_generics(funcs).is_some()
        && funcs.iter().all(|f| {
            f.sig.inputs.iter().all(|arg| match arg {
                FnArg::Typed(pt) => writable(&pt.ty),
                FnArg::Receiver(_) => false,
            })
        })
}

/// Whether the type contains `impl Trait`, and whether it names `Self`.
fn names_impl_trait_or_self(ty: &syn::Type) -> (bool, bool) {
    struct V {
        impl_trait: bool,
        self_ty: bool,
    }

    impl<'ast> syn::visit::Visit<'ast> for V {
        fn visit_type_impl_trait(&mut self, _: &'ast syn::TypeImplTrait) {
            self.impl_trait = true;
        }

        fn visit_path(&mut self, path: &'ast syn::Path) {
            if path.segments.iter().any(|s| s.ident == "Self") {
                self.self_ty = true;
            }
            syn::visit::visit_path(self, path);
        }
    }

    let mut v = V {
        impl_trait: false,
        self_ty: false,
    };
    syn::visit::Visit::visit_type(&mut v, ty);
    (v.impl_trait, v.self_ty)
}

/// The union of the members' generics and where-predicates, keyed by name.
///
/// `None` if two members declare one name with different bounds, or a generic is unused by every
/// parameter list (the seed enum would hit `E0392`).
fn shared_generics(funcs: &[ItemFn]) -> Option<(Vec<syn::GenericParam>, Vec<syn::WherePredicate>)> {
    /// Bounds per generic, as sets so spelling order and inline vs. `where` don't matter.
    type Asked = std::collections::BTreeMap<String, std::collections::BTreeSet<String>>;

    fn name(param: &syn::GenericParam) -> String {
        match param {
            syn::GenericParam::Lifetime(l) => format!("'{}", l.lifetime.ident),
            syn::GenericParam::Type(t) => t.ident.to_string(),
            syn::GenericParam::Const(c) => format!("const {}", c.ident),
        }
    }

    /// The generic a predicate bounds directly: `T` for `T: Copy`, `None` for `Vec<T>: Clone`.
    fn bounded_param(predicate: &syn::WherePredicate) -> Option<String> {
        match predicate {
            syn::WherePredicate::Type(t) => match &t.bounded_ty {
                syn::Type::Path(p) => p.path.get_ident().map(Ident::to_string),
                _ => None,
            },
            syn::WherePredicate::Lifetime(l) => Some(format!("'{}", l.lifetime.ident)),
            _ => None,
        }
    }

    /// Each generic's bounds, inline and `where`. A const generic's type counts as a bound.
    fn asked(func: &ItemFn) -> Asked {
        let mut asked = Asked::new();
        for param in &func.sig.generics.params {
            let bounds = asked.entry(name(param)).or_default();
            match param {
                syn::GenericParam::Lifetime(l) => bounds.extend(l.bounds.iter().map(pretty_type)),
                syn::GenericParam::Type(t) => bounds.extend(t.bounds.iter().map(pretty_type)),
                syn::GenericParam::Const(c) => {
                    bounds.insert(pretty_type(&c.ty));
                }
            }
        }
        for predicate in func
            .sig
            .generics
            .where_clause
            .iter()
            .flat_map(|w| &w.predicates)
        {
            let Some(of) = bounded_param(predicate) else {
                continue;
            };
            let Some(bounds) = asked.get_mut(&of) else {
                continue;
            };
            match predicate {
                syn::WherePredicate::Type(t) => bounds.extend(t.bounds.iter().map(pretty_type)),
                syn::WherePredicate::Lifetime(l) => bounds.extend(l.bounds.iter().map(pretty_type)),
                _ => {}
            }
        }
        asked
    }

    // Take each generic from its first declarer; later declarers must agree.
    let mut params: Vec<syn::GenericParam> = Vec::new();
    let mut predicates: Vec<syn::WherePredicate> = Vec::new();
    let mut agreed: Asked = Asked::new();
    for func in funcs {
        let asked = asked(func);
        for param in &func.sig.generics.params {
            let of = name(param);
            match agreed.get(&of) {
                Some(had) if had != &asked[&of] => return None,
                Some(_) => {}
                None => {
                    agreed.insert(of.clone(), asked[&of].clone());
                    params.push(param.clone());
                    predicates.extend(
                        func.sig
                            .generics
                            .where_clause
                            .iter()
                            .flat_map(|w| &w.predicates)
                            .filter(|p| bounded_param(p).as_deref() == Some(of.as_str()))
                            .cloned(),
                    );
                }
            }
        }
        // Other predicates are carried once, as written.
        let free: Vec<syn::WherePredicate> = func
            .sig
            .generics
            .where_clause
            .iter()
            .flat_map(|w| &w.predicates)
            .filter(|p| bounded_param(p).is_none())
            .cloned()
            .collect();
        for predicate in free {
            if !predicates
                .iter()
                .any(|had| pretty_type(had) == pretty_type(&predicate))
            {
                predicates.push(predicate);
            }
        }
    }

    // Every generic must appear in some parameter type.
    let mentioned: Vec<String> = funcs
        .iter()
        .flat_map(|f| &f.sig.inputs)
        .filter_map(|arg| match arg {
            FnArg::Typed(pt) => Some(pt.ty.to_token_stream().to_string()),
            FnArg::Receiver(_) => None,
        })
        .collect();
    let used = |param: &syn::GenericParam| {
        let bare = name(param);
        let bare = bare.trim_start_matches("const ");
        mentioned.iter().any(|ty| match param {
            syn::GenericParam::Lifetime(_) => ty.contains(bare),
            _ => ty
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|word| word == bare),
        })
    };
    params.iter().all(used).then_some((params, predicates))
}

/// A parameter type as a seed field: elided lifetimes become the seed lifetime, `Self` becomes
/// the concrete type.
fn seed_field_type(ty: &syn::Type, self_ty: Option<&syn::Type>) -> syn::Type {
    struct V<'a> {
        lt: syn::Lifetime,
        self_ty: Option<&'a syn::Type>,
    }

    impl syn::visit_mut::VisitMut for V<'_> {
        fn visit_type_reference_mut(&mut self, r: &mut syn::TypeReference) {
            if r.lifetime.is_none() {
                r.lifetime = Some(self.lt.clone());
            }
            syn::visit_mut::visit_type_reference_mut(self, r);
        }

        fn visit_lifetime_mut(&mut self, l: &mut syn::Lifetime) {
            if l.ident == "_" {
                *l = self.lt.clone();
            }
        }

        fn visit_type_mut(&mut self, ty: &mut syn::Type) {
            if let (syn::Type::Path(p), Some(concrete)) = (&*ty, self.self_ty)
                && p.qself.is_none()
                && p.path.is_ident("Self")
            {
                *ty = concrete.clone();
                return;
            }
            syn::visit_mut::visit_type_mut(self, ty);
        }
    }

    let mut ty = ty.clone();
    let mut v = V {
        lt: seed_lifetime(),
        self_ty,
    };
    syn::visit_mut::VisitMut::visit_type_mut(&mut v, &mut ty);
    ty
}

/// The parts of an expansion that are the same whichever way a group is emitted.
struct Pieces<'a> {
    /// The imports and the entry and frame enums.
    machinery: &'a TokenStream,
    /// The `#[allow(..)]` the rewritten body needs.
    allows: &'a TokenStream,
    /// One arm per entry point.
    arms: &'a [TokenStream],
    /// Continuation-frame dispatch.
    resume: &'a TokenStream,
    /// How each context slot is filled from the member's parameters.
    ctx_inits: &'a [TokenStream],
    /// `: R`, naming the driver's result type.
    ret_ann: &'a TokenStream,
    /// Type ascription for the entry's known payload types.
    anchor: &'a TokenStream,
    /// Annotation on the entry value, if any.
    input_ann: &'a TokenStream,
    /// `: Frames<Frame<..>>`.
    frames_ann: &'a TokenStream,
    /// The return-type union, declared outside the machine since its signature names it.
    ret_union_decl: &'a TokenStream,
}

/// Emit one shared machine for the group, with each member a call into it. See [`liftable`].
///
/// Members marked `inner` are written inside the machine; their output slot is empty.
fn lifted(
    funcs: &[ItemFn],
    ctx: &Ctx,
    pieces: &Pieces<'_>,
    self_ty: Option<&syn::Type>,
    methods: &[Option<MethodSplit>],
    inner: &[bool],
) -> syn::Result<(Vec<TokenStream>, TokenStream)> {
    let Pieces {
        machinery,
        allows,
        arms,
        resume,
        ctx_inits,
        anchor,
        input_ann,
        frames_ann,
        ret_ann,
        ret_union_decl,
    } = pieces;
    let members: Vec<Ident> = funcs.iter().map(|f| f.sig.ident.clone()).collect();
    let (seed_ty, machine, ctxp) = (seed_ty(&members), machine_fn(&members), ctx_param());
    let (entry, lt) = (entry_ty(), seed_lifetime());
    let ret = {
        let ann = ctx.ret_ann.clone();
        let ty = ann.into_iter().skip(1).collect::<TokenStream>();
        quote! { -> #ty }
    };

    // Seed variants: one per member, holding its parameters.
    let variants: Vec<TokenStream> = funcs
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let v = entry_variant(i);
            let tys = f.sig.inputs.iter().filter_map(|arg| match arg {
                FnArg::Typed(pt) => Some(seed_field_type(&pt.ty, self_ty)),
                FnArg::Receiver(_) => None,
            });
            quote! { #v(#(#tys),*) }
        })
        .collect();

    // Generics plus the seed lifetime (only if some field borrows).
    let (params, predicates) =
        shared_generics(funcs).expect("`liftable` said the members share their generics");
    let borrows = variants
        .iter()
        .any(|v| v.to_string().contains(&lt.to_string()));
    let lifetime = borrows.then(|| quote! { #lt });
    let args = params.iter().map(|param| match param {
        syn::GenericParam::Lifetime(l) => {
            let l = &l.lifetime;
            quote! { #l }
        }
        syn::GenericParam::Type(t) => {
            let t = &t.ident;
            quote! { #t }
        }
        syn::GenericParam::Const(c) => {
            let c = &c.ident;
            quote! { #c }
        }
    });
    let declared: Vec<TokenStream> = lifetime
        .iter()
        .cloned()
        .chain(params.iter().map(|p| quote! { #p }))
        .collect();
    let passed: Vec<TokenStream> = lifetime.iter().cloned().chain(args).collect();
    let (seed_generics, seed_args) = match declared.is_empty() {
        true => (TokenStream::new(), TokenStream::new()),
        false => (quote! { <#(#declared),*> }, quote! { <#(#passed),*> }),
    };
    let where_clause = match predicates.is_empty() {
        true => TokenStream::new(),
        false => quote! { where #(#predicates),* },
    };

    let dispatch = funcs.iter().enumerate().map(|(i, f)| {
        let v = entry_variant(i);
        let names: Vec<&Ident> = f
            .sig
            .inputs
            .iter()
            .filter_map(|arg| match arg {
                FnArg::Typed(pt) => match &*pt.pat {
                    Pat::Ident(p) => Some(&p.ident),
                    _ => None,
                },
                FnArg::Receiver(_) => None,
            })
            .collect();
        let p = ctx.member(i);
        let payload: Vec<TokenStream> = p
            .param_names
            .iter()
            .enumerate()
            .map(|(j, n)| {
                if p.pinned[j].get() {
                    quote! { ::core::ptr::from_ref(#n) }
                } else {
                    quote! { #n }
                }
            })
            .collect();
        let variant = entry_variant(i);
        quote! {
            #seed_ty::#v(#(#names),*) => ((#(#ctx_inits,)*), #entry::#variant((#(#payload,)*))),
        }
    });

    // Each member keeps its signature and calls the machine with its seed.
    let entries = funcs.iter().enumerate().map(|(i, f)| {
        let (attrs, vis) = (&f.attrs, &f.vis);
        let (outer, sig) = match &methods[i] {
            Some(m) => {
                let outer = &m.outer;
                let mut sig = f.sig.clone();
                sig.ident = m.inner.clone();
                (quote! { #outer }, sig)
            }
            None => (TokenStream::new(), f.sig.clone()),
        };
        let v = entry_variant(i);
        let names: Vec<&Ident> = f
            .sig
            .inputs
            .iter()
            .filter_map(|arg| match arg {
                FnArg::Typed(pt) => match &*pt.pat {
                    Pat::Ident(p) => Some(&p.ident),
                    _ => None,
                },
                FnArg::Receiver(_) => None,
            })
            .collect();
        let call = match self_ty {
            // A nested `fn` cannot name `Self` (E0401).
            Some(ty) if inner[i] => quote! { <#ty>::#machine },
            Some(_) => quote! { Self::#machine },
            None => quote! { #machine },
        };
        let take_out = ctx.take_result(i);
        quote! {
            #outer

            #(#attrs)*
            #[inline]
            #vis #sig {
                let __ss_out = #call(#seed_ty::#v(#(#names),*));
                #take_out
            }
        }
    });

    // Members declared in a body go inside the machine.
    let entries: Vec<TokenStream> = entries.collect();
    let within: Vec<&TokenStream> = entries
        .iter()
        .zip(inner)
        .filter(|&(_, &nested)| nested)
        .map(|(w, _)| w)
        .collect();

    let seed_decl = quote! {
        #ret_union_decl

        #[allow(non_camel_case_types)]
        enum #seed_ty #seed_generics #where_clause {
            #(#variants,)*
        }
    };
    let loop_expr = driver::machine(&quote! { __ss_entry }, input_ann, frames_ann, arms, resume);
    // Bodies run in the machine, so it must be `#[track_caller]` if any member is.
    let tracked = funcs
        .iter()
        .any(|f| f.attrs.iter().any(|a| a.path().is_ident("track_caller")))
        .then(|| quote! { #[track_caller] });
    let machine_decl = quote! {
        #allows
        #tracked
        fn #machine #seed_generics (__ss_seed: #seed_ty #seed_args) #ret #where_clause {
            #machinery
            #(#within)*

            let (mut #ctxp, __ss_entry) = match __ss_seed {
                #(#dispatch)*
            };
            #anchor
            let __ss_out #ret_ann = #loop_expr;
            __ss_out
        }
    };

    // In an impl block, the seed enum is hoisted out beside it.
    let (hoisted, with_first) = match self_ty {
        Some(_) => (seed_decl, machine_decl),
        None => (TokenStream::new(), quote! { #seed_decl #machine_decl }),
    };

    // Declarations go with the first member not written inside the machine.
    let beside = inner
        .iter()
        .position(|&nested| !nested)
        .expect("a cycle has a member written beside its driver");
    let out: Vec<TokenStream> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            if inner[i] {
                TokenStream::new()
            } else if i == beside {
                let with_first = &with_first;
                quote! { #with_first #entry }
            } else {
                entry.clone()
            }
        })
        .collect();
    Ok((out, hoisted))
}

/// The single name an item declares, if any (`use`, `impl` etc. give `None`).
fn item_name(item: &Item) -> Option<&Ident> {
    match item {
        Item::Const(i) => Some(&i.ident),
        Item::Enum(i) => Some(&i.ident),
        Item::ExternCrate(i) => Some(&i.ident),
        Item::Fn(i) => Some(&i.sig.ident),
        Item::Macro(i) => i.ident.as_ref(),
        Item::Mod(i) => Some(&i.ident),
        Item::Static(i) => Some(&i.ident),
        Item::Struct(i) => Some(&i.ident),
        Item::Trait(i) => Some(&i.ident),
        Item::TraitAlias(i) => Some(&i.ident),
        Item::Type(i) => Some(&i.ident),
        Item::Union(i) => Some(&i.ident),
        _ => None,
    }
}

/// One `Cell<bool>` per payload parameter, all false until `scan_pinned_args` runs.
fn param_names_len_cells(names: &[Ident]) -> Vec<Cell<bool>> {
    names.iter().map(|_| Cell::new(false)).collect()
}

/// A type rendered without token spacing (`&mut Vec<u64>`, not `& mut Vec < u64 >`).
fn pretty_type(ty: &impl ToTokens) -> String {
    let mut out = ty.to_token_stream().to_string();
    for (from, to) in [
        (" <", "<"),
        ("< ", "<"),
        (" >", ">"),
        ("> ", ">"),
        ("& ", "&"),
        (" ,", ","),
    ] {
        out = out.replace(from, to);
    }
    out
}

/// The return type alone; empty where [`ret_annotation`] is.
fn ret_bare_type(sig: &syn::Signature) -> TokenStream {
    match &sig.output {
        ReturnType::Default => quote! { () },
        ReturnType::Type(_, ty) if annotatable(ty) => quote! { #ty },
        ReturnType::Type(..) => quote! {},
    }
}

/// `: R` for the return type, so continuations can resolve methods on it (else E0689).
/// Empty if not [`annotatable`].
fn ret_annotation(sig: &syn::Signature) -> TokenStream {
    match &sig.output {
        ReturnType::Default => quote! { : () },
        ReturnType::Type(_, ty) if annotatable(ty) => quote! { : #ty },
        ReturnType::Type(..) => quote! {},
    }
}

/// Whether the type can annotate a `let`: not if it contains `impl Trait` (E0562) or is `!`.
fn annotatable(ty: &syn::Type) -> bool {
    !names_impl_trait_or_self(ty).0 && !matches!(peel_type(ty), syn::Type::Never(_))
}

fn reject_unsupported_signature(sig: &syn::Signature) -> syn::Result<()> {
    if let Some(a) = &sig.asyncness {
        return Err(syn::Error::new(
            a.span(),
            "`#[stack_safe]` does not support `async fn`: the rewritten body is a loop over \
             a frame stack, which an async state machine cannot hold without pinning",
        ));
    }
    if let Some(c) = &sig.constness {
        return Err(syn::Error::new(
            c.span(),
            "`#[stack_safe]` does not support `const fn`: the expansion allocates",
        ));
    }
    if sig.variadic.is_some() {
        return Err(syn::Error::new(
            sig.variadic.span(),
            "`#[stack_safe]` does not support variadics",
        ));
    }
    Ok(())
}

/// Build the group's [`Ctx`]: split parameters, check members agree, settle the result type,
/// and run the body scans code generation depends on.
fn analyse(
    funcs: &[ItemFn],
    opts: Opts,
    assoc: bool,
    self_ty: Option<&syn::Type>,
) -> syn::Result<Ctx> {
    // Calls are matched by name, so members must have distinct names.
    for (i, func) in funcs.iter().enumerate() {
        if funcs[..i].iter().any(|g| g.sig.ident == func.sig.ident) {
            return Err(syn::Error::new(
                func.sig.ident.span(),
                format!(
                    "`{}` is declared twice among the functions `#[stack_safe]` rewrites together. \
                     A recursive call is recognised by name, since a macro resolves no paths, so \
                     the two cannot be told apart and calls to either would enter the same body. \
                     Rename one of them",
                    func.sig.ident,
                ),
            ));
        }
    }

    let invariant_contexts = invariant_shared_contexts(funcs, assoc, self_ty);
    let mut splits = Vec::new();
    for (func, invariant_context) in funcs.iter().zip(&invariant_contexts) {
        reject_unsupported_signature(&func.sig)?;
        splits.push(split_params(func, self_ty, invariant_context)?);
    }

    // Members share one context tuple.
    let first = &splits[0];
    for (split, func) in splits.iter().zip(funcs).skip(1) {
        if split.slot_keys != first.slot_keys {
            return Err(syn::Error::new(
                func.sig.span(),
                format!(
                    "`{}` and `{}` are mutually recursive, so they share one driver and must take \
                     the same `&mut` parameters (in any position); `{}` takes [{}] and `{}` takes \
                     [{}]",
                    funcs[0].sig.ident,
                    func.sig.ident,
                    funcs[0].sig.ident,
                    first.slot_types.join(", "),
                    func.sig.ident,
                    split.slot_types.join(", "),
                ),
            ));
        }
    }

    // Differing return types are joined into a union enum.
    let rets: Vec<TokenStream> = funcs.iter().map(|f| ret_annotation(&f.sig)).collect();
    let ret_types: Vec<TokenStream> = funcs.iter().map(|f| ret_bare_type(&f.sig)).collect();
    let differ = rets.iter().any(|r| r.to_string() != rets[0].to_string());
    // `impl Trait` returns can't be named, so they are only allowed for a lone function.
    if funcs.len() > 1
        && let Some((_, f)) = funcs.iter().enumerate().find(|(i, _)| rets[*i].is_empty())
    {
        return Err(syn::Error::new(
            f.sig.output.span(),
            format!(
                "`{}` is part of a mutually recursive group, and a group's members answer \
                 through one driver, so their return types have to be nameable. An `impl \
                 Trait` return is its own opaque type and cannot be named, not even to join \
                 it with another member's. Return a concrete type, or box it",
                f.sig.ident,
            ),
        ));
    }
    let ret_union = differ.then(ret_union_ty);
    let ret_ann = match &ret_union {
        None => rets[0].clone(),
        Some(union) => {
            let tys = funcs.iter().map(|f| match &f.sig.output {
                ReturnType::Type(_, ty) => quote! { #ty },
                ReturnType::Default => quote! { () },
            });
            quote! { : #union<#(#tys),*> }
        }
    };
    let ctx = Ctx {
        assoc,
        members: splits.iter().map(|s| s.member.clone()).collect(),
        counter: Cell::new(0),
        loops: RefCell::new(Vec::new()),
        resumes: RefCell::new(Vec::new()),
        results: RefCell::new(Vec::new()),
        context: splits.into_iter().next().expect("non-empty").context,
        ret_ann: ret_ann.clone(),
        rets: rets.clone(),
        ret_types,
        ret_union: ret_union.clone(),
        opts,
        current: Cell::new(0),
        gates: RefCell::new(Vec::new()),
        local_types: RefCell::new(HashMap::new()),
        locals: RefCell::new(HashSet::new()),
        asked_stores: RefCell::new(Vec::new()),
        hoist: Cell::new(false),
    };

    reject_generic_payload(&ctx, funcs)?;
    ctx.hoist.set(checks_are_shareable(&ctx, funcs));

    for (i, func) in funcs.iter().enumerate() {
        // Must precede the scans.
        ctx.current.set(i);
        note_annotated_lets(&ctx, &func.block);
        validate(&ctx, func)?;
        reject_shadowed_across_a_call(&ctx, func)?;
        // Both scans must run before codegen: they decide which slots and payload positions
        // become raw pointers.
        scan_context_args(&ctx, &func.block)?;
        scan_pinned_args(&ctx, &func.block)?;
    }
    Ok(ctx)
}

/// Whether all resume arms can share one `?` check (see `driver::resume`). Requires every
/// recursive call to be under `?`, a single return type, and no unsafe options (their cleanup
/// would be skipped on the error path).
fn checks_are_shareable(ctx: &Ctx, funcs: &[ItemFn]) -> bool {
    struct V<'a> {
        ctx: &'a Ctx,
        shareable: bool,
    }

    impl<'ast> syn::visit::Visit<'ast> for V<'_> {
        fn visit_expr(&mut self, e: &'ast syn::Expr) {
            // `rec(..)?` is fine; still check its arguments.
            if let syn::Expr::Try(t) = e
                && let Some((_, call)) = self.ctx.rec_call(strip_parens(&t.expr))
            {
                for arg in &call.args {
                    self.visit_expr(arg);
                }
                return;
            }
            if self.ctx.rec_call(e).is_some() {
                self.shareable = false;
                return;
            }
            syn::visit::visit_expr(self, e);
        }

        fn visit_item(&mut self, _: &'ast Item) {}
    }

    if ctx.ret_union.is_some() || !ctx.opts.same_rewrite(&Opts::default()) {
        return false;
    }
    let mut v = V {
        ctx,
        shareable: true,
    };
    for (i, func) in funcs.iter().enumerate() {
        ctx.current.set(i);
        syn::visit::Visit::visit_block(&mut v, &func.block);
    }
    v.shareable
}

/// Lower each member's body to its entry arm, hoisting out the items bodies declare.
/// Hoisted items share one scope, so two members declaring the same name is an error.
fn member_arms(ctx: &Ctx, funcs: &[ItemFn]) -> syn::Result<(Vec<Item>, Vec<TokenStream>)> {
    let mut declared: HashMap<String, &Ident> = HashMap::new();
    let mut items: Vec<Item> = Vec::new();
    let mut main_arms: Vec<TokenStream> = Vec::new();
    let entry = entry_ty();

    for (i, func) in funcs.iter().enumerate() {
        let (item_stmts, stmts): (Vec<&Stmt>, Vec<&Stmt>) = func
            .block
            .stmts
            .iter()
            .partition(|s| matches!(s, Stmt::Item(_)));
        for stmt in &item_stmts {
            let Stmt::Item(item) = stmt else {
                unreachable!("partitioned on Stmt::Item")
            };
            let Some(name) = item_name(item) else {
                continue;
            };
            if let Some(other) = declared.insert(name.to_string(), &func.sig.ident)
                && other != &func.sig.ident
            {
                return Err(syn::Error::new(
                    name.span(),
                    format!(
                        "`{other}` and `{}` are mutually recursive, so their bodies become arms of \
                         one `match`, and an item a body declares is moved out to one place \
                         enclosing all the arms. Both declare `{name}`, so it would be declared \
                         twice there. Rename one, or move it to the enclosing module",
                        func.sig.ident,
                    ),
                ));
            }
        }
        items.extend(item_stmts.into_iter().map(|s| match s {
            Stmt::Item(item) => item.clone(),
            _ => unreachable!("partitioned on Stmt::Item"),
        }));

        let stmts: Vec<Stmt> = stmts.into_iter().cloned().collect();
        let env = Env {
            wrap: ctx
                .ret_union
                .as_ref()
                .map(|u| (u.clone(), entry_variant(i))),
            scope: ctx.member(i).param_names.clone(),
            lp: None,
            restores: TokenStream::new(),
            teardown: TokenStream::new(),
            derived: Vec::new(),
        };
        let done = |v: TokenStream| -> syn::Result<TokenStream> {
            Ok(driver::done(ctx.wrap_result(i, v)))
        };
        ctx.current.set(i);
        let arm = cps_stmts(ctx, &env, &stmts, &done)?;
        let variant = entry_variant(i);
        let pats = &ctx.member(i).param_pats;
        let p = ctx.member(i);
        let anns: Vec<&TokenStream> = (0..p.param_anns.len())
            .map(|j| {
                if p.pinned[j].get() {
                    &p.param_anns_pinned[j]
                } else {
                    &p.param_anns[j]
                }
            })
            .collect();
        let prologue = ctx.ctx_prologue();
        main_arms.push(quote! {
            #entry::#variant((#(#pats,)*)) => {
                #prologue #(#anns)* #arm
            },
        });
    }

    Ok((items, main_arms))
}

/// Expand one group. `inner[i]` marks member `i` as declared in another member's body; such
/// members are written inside the shared driver.
pub(super) fn expand_group(
    funcs: Vec<ItemFn>,
    opts: Opts,
    self_ty: Option<&syn::Type>,
    inner: &[bool],
    assoc: bool,
) -> syn::Result<(Vec<TokenStream>, TokenStream)> {
    assert!(!funcs.is_empty(), "a group has at least one member");
    debug_assert_eq!(funcs.len(), inner.len(), "one placement flag per member");

    let mut funcs = funcs;

    // Desugar receivers into typed `Self` parameters plus a wrapper method.
    let group_names: Vec<Ident> = funcs.iter().map(|f| f.sig.ident.clone()).collect();
    let mut methods: Vec<Option<MethodSplit>> = Vec::with_capacity(funcs.len());
    for func in &mut funcs {
        // `impl Trait` params become generics; destructuring params get names.
        desugar_apit(func);
        desugar_param_patterns(func)?;
        methods.push(desugar_receiver(func, &group_names)?);
    }

    let lift = liftable(&funcs, self_ty.is_some());
    // A member declared in a body needs a lifted, non-generic driver: it can't hold its own copy
    // of the machine, and can't name the host's generics.
    let generic = lift && shared_generics(&funcs).is_some_and(|(params, _)| !params.is_empty());
    if (!lift || generic)
        && let Some((f, _)) = funcs.iter().zip(inner).find(|&(_, &nested)| nested)
    {
        let name = &f.sig.ident;
        let why = if generic {
            "and the driver they share is generic, which a function declared in a body cannot \
             be: it cannot name the parameters of the one hosting it"
        } else {
            "and this group cannot be written as one shared driver: a member takes an `impl \
             Trait` parameter, or names a `Self` the driver's signature cannot spell"
        };
        return Err(syn::Error::new(
            name.span(),
            format!(
                "`{name}` is declared inside the body of a function it recurses with, so the \
                 driver they share has to be written outside that body — {why}. Move `{name}` out \
                 to the enclosing scope, where it can be a member like any other"
            ),
        ));
    }

    let ctx = analyse(&funcs, opts, assoc, self_ty)?;

    let (items, main_arms) = member_arms(&ctx, &funcs)?;
    let entry = entry_ty();

    let mut ctx_inits: Vec<TokenStream> = ctx.context.iter().map(CtxEntry::init_expr).collect();
    let pin = pin_ty();
    // Positions with unnamable pointee types get their own store.
    for _ in 0..ctx.own_store_count() {
        ctx_inits.push(quote! { #pin::new() });
    }
    // The rest share one store of an enum with named element types, so inference works across
    // members and `#[cfg]`ed variants.
    let shared_elements = ctx.shared_elements();
    let pinned_decl = if shared_elements.is_empty() {
        TokenStream::new()
    } else {
        let en = pinned_ty();
        ctx_inits.push(quote! { #pin::<#en<#(#shared_elements),*>>::new() });
        let params: Vec<Ident> = (0..shared_elements.len()).map(pinned_param).collect();
        let variants: Vec<Ident> = (0..shared_elements.len()).map(pinned_variant).collect();
        quote! {
            #[allow(dead_code)]
            enum #en<#(#params),*> {
                #(#variants(#params),)*
            }
        }
    };
    let loop_base = ctx.loop_base();
    let loops = ctx.loops.take();
    let resumes = ctx.resumes.take();
    let Solved {
        states,
        frames,
        derived,
    } = solve_payloads(&loops, &resumes);
    // See `walk::Derived`.
    let recompute = |n: usize| -> TokenStream {
        let binds = derived[n].iter().map(|d| {
            let (name, expr) = (&d.name, &d.expr);
            quote! { let #name = #expr; }
        });
        quote! { #(#binds)* }
    };

    // Whether a seed lifetime exists (lifted, and some parameter is a reference).
    let seed_lt = lift
        && ctx
            .members
            .iter()
            .any(|m| m.param_pointees.iter().any(Option::is_some));
    let mut subst = HashMap::new();
    for (n, st) in states.iter().enumerate() {
        subst.insert(
            state_marker(n).to_string(),
            payload_expr(&ctx, loops[n].member, st, self_ty, seed_lt),
        );
    }
    for (r, fr) in frames.iter().enumerate() {
        subst.insert(
            frame_marker(r).to_string(),
            payload_expr(&ctx, resumes[r].point.member, fr, self_ty, seed_lt),
        );
    }

    let frame = frame_ty();

    let mut arms = main_arms;
    for (n, lp) in loops.iter().enumerate() {
        let v = entry_variant(loop_base + n);
        let st = &states[n];
        let code = &lp.code;
        let recomputed = recompute(n);
        let prologue = ctx.ctx_prologue();
        let (gate, stand_in) = gating(&lp.gates);
        // A gated variant still needs an arm when the gate is off; its payload is `()`.
        let stand_in = stand_in.map(|ungated| {
            quote! {
                #ungated
                #entry::#v(()) => unreachable!("gated out"),
            }
        });
        arms.push(quote! {
            #gate
            #entry::#v((#(mut #st,)*)) => {
                #recomputed #prologue #code
            },
            #stand_in
        });
    }
    // Shared `?` check before frame dispatch; see `checks_are_shareable` and `driver::resume`.
    let hoist = ctx.hoist.get() && !resumes.is_empty();
    assert!(
        !hoist || resumes.iter().all(|r| r.checked.get()),
        "stack_safe: the shared carrier check was decided on but some resume point did not take it"
    );
    // One arm per recursive call site.
    let mut frame_arms: Vec<TokenStream> = Vec::new();
    // Drops each frame when the shared check fails.
    let mut frame_drops: Vec<TokenStream> = Vec::new();
    for (r, res) in resumes.iter().enumerate() {
        let variant = frame_variant(r);
        let payload = &frames[r];
        let value = &res.value;
        let code = &res.point.code;
        let recomputed = recompute(loops.len() + r);
        let (gate, stand_in) = gating(&res.point.gates);
        if hoist {
            let ok = ok_local();
            let stand_in_drop = stand_in.clone();
            let stand_in = stand_in.map(|ungated| {
                quote! { #ungated #frame::#variant(()) => unreachable!("gated out"), }
            });
            frame_arms.push(quote! {
                #gate
                #frame::#variant((#(mut #payload,)*)) => { let #value = #ok; #recomputed #code },
                #stand_in
            });
            let dropped = payload.iter().rev();
            let stand_in_drop = stand_in_drop.map(|ungated| {
                quote! { #ungated #frame::#variant(()) => {}, }
            });
            frame_drops.push(quote! {
                #gate
                #frame::#variant((#(#payload,)*)) => { #(::core::mem::drop(#dropped);)* },
                #stand_in_drop
            });
            continue;
        }
        // See `driver::resume_direct`.
        let resumed = value_local();
        let stand_in = stand_in.map(|ungated| {
            quote! { #ungated #frame::#variant(()) => unreachable!("gated out"), }
        });
        frame_arms.push(quote! {
            #gate
            #frame::#variant((#(mut #payload,)*)) => { let #value = #resumed; #recomputed #code },
            #stand_in
        });
    }
    let resume = match hoist {
        true => driver::resume(&frame_arms, &frame_drops),
        false => driver::resume_direct(&frame_arms),
    };
    let arms: Vec<TokenStream> = arms.into_iter().map(|ts| substitute(ts, &subst)).collect();
    let resume = substitute(resume, &subst);

    let total_entries = loop_base + loops.len();
    let entry_params: Vec<Ident> = (0..total_entries)
        .map(|n| format_ident!("__SsA{}", n))
        .collect();
    let entry_variants: Vec<Ident> = (0..total_entries).map(entry_variant).collect();
    let frame_params: Vec<Ident> = (0..resumes.len())
        .map(|r| format_ident!("__SsF{}", r))
        .collect();
    let frame_variants: Vec<Ident> = (0..resumes.len()).map(frame_variant).collect();
    let ctxp = ctx_param();

    // Ascribe known entry payload types up front; inference alone can't pin loop states.
    let anchor = {
        let entry_args = (0..total_entries)
            .map(|n| variant_payload_type(&ctx, n, &loops, &states, self_ty, seed_lt));
        let entry_ty_name = entry_ty();
        quote! { let _: &#entry_ty_name<#(#entry_args),*> = &__ss_entry; }
    };
    // Name frame slot types on the frame stack, else weakly constrained slots stay ambiguous.
    // Gated frames are `_`: their type depends on the `#[cfg]`.
    let frame_named = {
        let frame_args: Vec<TokenStream> = frames
            .iter()
            .enumerate()
            .map(|(r, fr)| {
                let point = &resumes[r].point;
                match point.gates.is_empty() {
                    true => slots_payload_type(&ctx, point.member, fr, self_ty, seed_lt),
                    false => quote! { _ },
                }
            })
            .collect();
        let frame_ty_name = frame_ty();
        match frame_args.is_empty() {
            true => quote! { #frame_ty_name },
            false => quote! { #frame_ty_name<#(#frame_args),*> },
        }
    };
    let input_ann = TokenStream::new();
    let frames_ann = {
        let frames_ty_name = frames_ty();
        quote! { : #frames_ty_name<#frame_named> }
    };

    // Import only the `defs` items the generated code uses.
    let defs_imports = defs_imports(&quote! { #(#arms)* #resume #(#ctx_inits)* });
    let ret_union_decl = match &ctx.ret_union {
        None => TokenStream::new(),
        Some(union) => {
            let params: Vec<Ident> = (0..funcs.len())
                .map(|i| format_ident!("__SsR{}", i))
                .collect();
            let variants: Vec<Ident> = (0..funcs.len()).map(entry_variant).collect();
            quote! {
                enum #union<#(#params),*> {
                    #(#variants(#params),)*
                }
            }
        }
    };
    let machinery = quote! {
        #defs_imports
        #(#items)*

        #pinned_decl

        enum #entry<#(#entry_params),*> {
            #(#entry_variants(#entry_params),)*
        }

        enum #frame<#(#frame_params),*> {
            #(#frame_variants(#frame_params),)*
        }
    };
    // Generated code trips these legitimately.
    let allows = quote! {
        #[allow(
            unused_mut,
            unused_variables,
            unused_parens,
            unused_assignments,
            unreachable_code,
            clippy::diverging_sub_expression,
            clippy::drop_non_drop
        )]
    };

    if lift {
        let pieces = Pieces {
            machinery: &machinery,
            allows: &allows,
            arms: &arms,
            resume: &resume,
            ctx_inits: &ctx_inits,
            anchor: &anchor,
            input_ann: &input_ann,
            frames_ann: &frames_ann,
            ret_ann: &ctx.ret_ann,
            ret_union_decl: &ret_union_decl,
        };
        return lifted(&funcs, &ctx, &pieces, self_ty, &methods, inner);
    }

    // Not liftable: each member gets its own copy of the machine.
    let ret_ann = &ctx.ret_ann;
    let loop_expr = driver::machine(
        &quote! { __ss_entry },
        &input_ann,
        &frames_ann,
        &arms,
        &resume,
    );
    let mut out = Vec::with_capacity(funcs.len());
    for (i, func) in funcs.iter().enumerate() {
        let attrs = &func.attrs;
        let vis = &func.vis;
        let variant = entry_variant(i);
        let take_out = ctx.take_result(i);
        let p = ctx.member(i);
        let seed: Vec<TokenStream> = p
            .param_names
            .iter()
            .enumerate()
            .map(|(j, n)| {
                if p.pinned[j].get() {
                    quote! { ::core::ptr::from_ref(#n) }
                } else {
                    quote! { #n }
                }
            })
            .collect();

        let (wrapper, sig) = match &methods[i] {
            Some(m) => {
                let outer = &m.outer;
                let mut sig = func.sig.clone();
                sig.ident = m.inner.clone();
                (quote! { #outer }, sig)
            }
            None => (TokenStream::new(), func.sig.clone()),
        };

        out.push(quote! {
            #wrapper

            #(#attrs)*
            #[allow(
                unused_mut,
                unused_variables,
                unused_parens,
                unused_assignments,
                unreachable_code,
                clippy::diverging_sub_expression,
                clippy::drop_non_drop
            )]
            #vis #sig {
                #defs_imports
                #(#items)*

                #ret_union_decl

                #pinned_decl

                enum #entry<#(#entry_params),*> {
                    #(#entry_variants(#entry_params),)*
                }

                enum #frame<#(#frame_params),*> {
                    #(#frame_variants(#frame_params),)*
                }

                let mut #ctxp = (#(#ctx_inits,)*);
                let __ss_entry = #entry::#variant((#(#seed,)*));
                let __ss_out #ret_ann = #loop_expr;
                #take_out
            }
        });
    }
    Ok((out, TokenStream::new()))
}

/// For `#[cfg]` gates: the arm's `cfg`, and the `cfg(not(..))` for its stand-in arm.
/// Ungated arms get neither.
fn gating(gates: &[TokenStream]) -> (TokenStream, Option<TokenStream>) {
    match gates {
        [] => (TokenStream::new(), None),
        _ => (
            quote! { #[cfg(all(#(#gates),*))] },
            Some(quote! { #[cfg(not(all(#(#gates),*)))] }),
        ),
    }
}

/// A payload tuple's contents, ascribing each slot's type where known (else inference fails).
fn payload_expr(
    ctx: &Ctx,
    member: usize,
    ids: &[Ident],
    self_ty: Option<&syn::Type>,
    seed_lt: bool,
) -> TokenStream {
    let parts = ids.iter().map(|id| match ctx.slot_type(member, id) {
        Some(ty) => {
            let ty = match (seed_lt, syn::parse2::<syn::Type>(ty.clone())) {
                (true, Ok(parsed)) => {
                    let named = seed_field_type(&parsed, self_ty);
                    quote! { #named }
                }
                _ => ty,
            };
            quote! { { let __ss_slot: #ty = #id; __ss_slot }, }
        }
        None => quote! { #id, },
    });
    quote! { #(#parts)* }
}

/// The type an initializer states itself: a cast, a suffixed literal, `bool` or `char`.
fn self_typing(e: &syn::Expr) -> Option<TokenStream> {
    match e {
        syn::Expr::Cast(c) => {
            let ty = &c.ty;
            Some(quote! { #ty })
        }
        syn::Expr::Lit(l) => match &l.lit {
            syn::Lit::Int(i) if !i.suffix().is_empty() => {
                let ty = format_ident!("{}", i.suffix());
                Some(quote! { #ty })
            }
            syn::Lit::Float(f) if !f.suffix().is_empty() => {
                let ty = format_ident!("{}", f.suffix());
                Some(quote! { #ty })
            }
            syn::Lit::Bool(_) => Some(quote! { bool }),
            syn::Lit::Char(_) => Some(quote! { char }),
            _ => None,
        },
        syn::Expr::Group(g) => self_typing(&g.expr),
        syn::Expr::Paren(p) => self_typing(&p.expr),
        _ => None,
    }
}

/// Record locals bound by `let`, and each local's type where all its annotated or self-typed
/// bindings agree, so [`payload_expr`] can name its slot. A name bound by a pattern (`for`,
/// `if let`, match arm, destructuring `let`) or by `let x;` is ruled out.
fn note_annotated_lets(ctx: &Ctx, block: &syn::Block) {
    use std::collections::HashSet;

    #[derive(Default)]
    struct Found {
        annotated: HashMap<String, Vec<(String, TokenStream)>>,
        bound: HashSet<String>,
        poisoned: HashSet<String>,
    }

    struct V(Found);

    impl V {
        fn poison(&mut self, pat: &Pat) {
            for id in super::analyze::pat_bindings(pat) {
                self.0.poisoned.insert(id.to_string());
            }
        }
    }

    impl syn::visit::Visit<'_> for V {
        fn visit_local(&mut self, local: &syn::Local) {
            match &local.pat {
                Pat::Type(pt) if matches!(&*pt.pat, Pat::Ident(_)) => {
                    let Pat::Ident(id) = &*pt.pat else {
                        unreachable!("matched just above")
                    };
                    let ty = &pt.ty;
                    let ty = quote! { #ty };
                    self.0.bound.insert(id.ident.to_string());
                    self.0
                        .annotated
                        .entry(id.ident.to_string())
                        .or_default()
                        .push((ty.to_string(), ty));
                }
                Pat::Ident(id) if local.init.is_some() => {
                    self.0.bound.insert(id.ident.to_string());
                    if let Some(ty) = local.init.as_ref().and_then(|i| self_typing(&i.expr)) {
                        self.0
                            .annotated
                            .entry(id.ident.to_string())
                            .or_default()
                            .push((ty.to_string(), ty));
                    }
                }
                Pat::Ident(id) => match local.init.as_ref().and_then(|i| self_typing(&i.expr)) {
                    Some(ty) => self
                        .0
                        .annotated
                        .entry(id.ident.to_string())
                        .or_default()
                        .push((ty.to_string(), ty)),
                    None => self.poison(&local.pat),
                },
                other => self.poison(other),
            }
            syn::visit::visit_local(self, local);
        }

        fn visit_expr_for_loop(&mut self, f: &syn::ExprForLoop) {
            self.poison(&f.pat);
            syn::visit::visit_expr_for_loop(self, f);
        }

        fn visit_expr_let(&mut self, l: &syn::ExprLet) {
            self.poison(&l.pat);
            syn::visit::visit_expr_let(self, l);
        }

        fn visit_arm(&mut self, a: &syn::Arm) {
            self.poison(&a.pat);
            syn::visit::visit_arm(self, a);
        }

        fn visit_item(&mut self, _: &syn::Item) {}
    }

    let mut v = V(Found::default());
    syn::visit::Visit::visit_block(&mut v, block);
    let Found {
        annotated,
        bound,
        poisoned,
    } = v.0;
    for name in &bound {
        if !poisoned.contains(name) {
            ctx.note_local(&format_ident!("{name}"));
        }
    }
    for (name, tys) in annotated {
        if poisoned.contains(&name) {
            continue;
        }
        let first = &tys[0].0;
        if tys.iter().all(|(rendered, _)| rendered == first) {
            ctx.note_local_type(
                &format_ident!("{}", name),
                tys.into_iter().next().expect("non-empty").1,
            );
        }
    }
}

/// The payload type of one entry variant (member or loop), `_` for unknown slots.
fn variant_payload_type(
    ctx: &Ctx,
    variant: usize,
    loops: &[super::walk::PayloadPoint],
    states: &[Vec<Ident>],
    self_ty: Option<&syn::Type>,
    seed_lt: bool,
) -> TokenStream {
    let members = ctx.members.len();
    if variant < members {
        let member = ctx.member(variant);
        let tys = (0..member.param_names.len()).map(|j| {
            let known = match member.pinned[j].get() {
                // Pointer into the pinned store.
                true => member.param_pointees[j]
                    .clone()
                    .map(|elem| quote! { *const #elem }),
                false => member
                    .param_bare_types
                    .get(j)
                    .filter(|bare| !bare.is_empty())
                    .map(|bare| named(bare.clone(), self_ty, seed_lt)),
            };
            known.unwrap_or_else(|| quote! { _ })
        });
        return quote! { (#(#tys,)*) };
    }
    let n = variant - members;
    // Gated: left to inference.
    if !loops[n].gates.is_empty() {
        return quote! { _ };
    }
    slots_payload_type(ctx, loops[n].member, &states[n], self_ty, seed_lt)
}

/// The payload type of a tuple of slots, with `_` for any slot the macro cannot name.
fn slots_payload_type(
    ctx: &Ctx,
    member: usize,
    ids: &[Ident],
    self_ty: Option<&syn::Type>,
    seed_lt: bool,
) -> TokenStream {
    let tys = ids.iter().map(|id| match ctx.slot_type(member, id) {
        Some(ty) => named(ty, self_ty, seed_lt),
        None => quote! { _ },
    });
    quote! { (#(#tys,)*) }
}

/// A slot type with elided lifetimes set to the seed lifetime, if there is one.
fn named(ty: TokenStream, self_ty: Option<&syn::Type>, seed_lt: bool) -> TokenStream {
    match (seed_lt, syn::parse2::<syn::Type>(ty.clone())) {
        (true, Ok(parsed)) => {
            let ty = seed_field_type(&parsed, self_ty);
            quote! { #ty }
        }
        _ => ty,
    }
}
