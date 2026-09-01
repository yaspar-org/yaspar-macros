// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Assembling one cycle's expansion: split the parameters, run the transform, and emit the
//! entry enum, the frame enum, the driver's `match` over them, and a rewritten function per
//! member. Which cycles there are, and where each one's driver goes, is `scan.rs`.

use proc_macro2::{Ident, TokenStream};
use quote::{ToTokens, format_ident, quote};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use syn::spanned::Spanned;
use syn::{FnArg, Item, ItemFn, Pat, PatIdent, ReturnType, Stmt};

use super::Opts;
use super::analyze::{
    MethodSplit, desugar_receiver, scan_context_args, scan_pinned_args, validate,
};
use super::context::CtxEntry;
use super::cps::cps_stmts;
use super::loop_state::{solve_payloads, substitute};
use super::names::*;
use super::walk::{Ctx, Env, Member};

/// What one function contributes to its group's driver.
struct Split {
    context: Vec<CtxEntry>,
    member: Member,
    /// Token form of each context slot's declared type, so a group can check that
    /// its members agree on them.
    slot_types: Vec<String>,
}

/// Parameters split two ways. A `&mut` parameter (and any receiver) becomes a
/// *context* slot the driver owns and lends out; everything else travels in the
/// argument payload. Shared references are `Copy`, so the payload is fine for
/// them.
///
/// Payload parameters must be plain (optionally `mut`) idents: the expansion has
/// to rebuild the argument tuple as an *expression*, which is impossible from a
/// destructuring pattern.
/// Callers run [`reject_unsupported_signature`] first, so the signature is known
/// to be one the transform can handle.
fn split_params(func: &ItemFn) -> syn::Result<Split> {
    let sig = &func.sig;

    let mut param_pats = Vec::new();
    let mut param_anns: Vec<TokenStream> = Vec::new();
    let mut param_anns_pinned: Vec<TokenStream> = Vec::new();
    let mut param_pointees: Vec<Option<TokenStream>> = Vec::new();
    let mut param_types: Vec<TokenStream> = Vec::new();
    let mut param_names = Vec::new();
    let mut context: Vec<CtxEntry> = Vec::new();
    let mut context_at: HashMap<usize, usize> = HashMap::new();
    let mut slot_types: Vec<String> = Vec::new();
    let mut arg_index = 0usize;
    for arg in &sig.inputs {
        match arg {
            // `desugar_receiver` has already turned any receiver into a typed
            // parameter, so this is unreachable in practice.
            FnArg::Receiver(r) => {
                return Err(syn::Error::new(
                    r.span(),
                    "`#[stack_safe]` could not rewrite this receiver into an ordinary parameter",
                ));
            }
            FnArg::Typed(pt) => {
                let Pat::Ident(PatIdent {
                    ident,
                    by_ref: None,
                    subpat: None,
                    mutability,
                    ..
                }) = &*pt.pat
                else {
                    return Err(syn::Error::new(
                        pt.pat.span(),
                        "`#[stack_safe]` requires plain identifier parameters; bind the pattern \
                         inside the body instead",
                    ));
                };
                let is_mut_ref =
                    matches!(&*pt.ty, syn::Type::Reference(r) if r.mutability.is_some());
                if is_mut_ref {
                    if let Some(m) = mutability {
                        return Err(syn::Error::new(
                            m.span(),
                            "`#[stack_safe]` does not support a `mut` binding for a `&mut` \
                             parameter: the parameter becomes a context slot that every step \
                             re-derives, so reassigning the binding would not be visible",
                        ));
                    }
                    context_at.insert(arg_index, context.len());
                    slot_types.push(pretty_type(&pt.ty));
                    let ty = &pt.ty;
                    context.push(CtxEntry {
                        name: ident.clone(),
                        mutable: true,
                        init: quote! { #ident },
                        ty: quote! { #ty },
                        raw: Cell::new(false),
                    });
                } else {
                    param_pats.push(quote! { #mutability #ident });
                    param_names.push(ident.clone());
                    // `impl Trait` is the one parameter type that cannot be
                    // written down; such a parameter goes unpinned.
                    let ty = &pt.ty;
                    param_types.push(if matches!(&**ty, syn::Type::ImplTrait(_)) {
                        TokenStream::new()
                    } else {
                        quote! { : #ty }
                    });
                    param_pointees.push(match &**ty {
                        syn::Type::Reference(r) => {
                            let elem = &r.elem;
                            Some(quote! { #elem })
                        }
                        _ => None,
                    });
                    param_anns.push(if matches!(&**ty, syn::Type::ImplTrait(_)) {
                        TokenStream::new()
                    } else {
                        quote! { let #mutability #ident: #ty = #ident; }
                    });
                    // Used only if `scan_pinned_args` marks this position: the payload
                    // then holds a pointer into the driver's pinned store. The pointer's
                    // own type is named first, for the same reason the ordinary case
                    // names the reference's: nothing else fixes this payload's type, and
                    // in a group one member's payload is only ever built inside another
                    // member's arm.
                    //
                    // SAFETY: as in `CtxEntry::rebind`, this lands in the caller's crate
                    // and the invariant is ours. The pointer is one `Pin::push` returned
                    // for a value moved into the driver's store, and `Pin` never moves a
                    // value it holds, so the address stays valid. The frame that pushed
                    // it holds the mark that drops it, so the value outlives every arm
                    // that can reach this payload and is dropped once. Gated behind
                    // `data_in_frame`; covered by `tests/transform.rs` and
                    // `tests/group.rs` under both of Miri's aliasing models.
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
        },
        slot_types,
    })
}

/// Can this group share one machine, instead of a copy per member?
///
/// The machine can be lifted into a sibling function as long as that function's signature can be
/// written, and the only thing it has to name is a *seed*: one variant per member carrying that
/// member's own parameters. The entry and frame enums stay nested inside it, so their payloads — a
/// loop's state, a resume point's locals — are still inferred, and a loop is therefore no obstacle.
///
/// What cannot be written is what rules a group out:
///
/// - a lone member, which has nothing to share. A cycle of one is its own outermost member, so it
///   never holds a member that came out of a body — the case that has no other shape;
/// - `impl Trait` in a parameter, which cannot be an enum field at all: it would have to become a
///   generic parameter, which is a rewrite of the signature rather than a copy of it;
/// - `Self`, unless the caller supplies the concrete type it stands for, which it can when the
///   group came from an impl block without generics of its own;
/// - generic parameters the members cannot share — see [`shared_generics`].
///
/// One shape slips through, because it cannot be told apart from an ordinary type: a parameter
/// written as a bare path that *hides* a reference, such as an alias
/// `type Words<'a> = &'a [&'a str]` used as `w: Words`. Nothing in the tokens says a lifetime is
/// elided there, so the seed field is emitted verbatim and the enum has no lifetime to give it,
/// which is an `E0106` on the parameter. Writing the elision out, `w: Words<'_>`, both fixes it and
/// keeps the group lifted.
fn liftable(funcs: &[ItemFn], has_self_ty: bool) -> bool {
    // A `dyn` type needs no test: bare, it is unsized and could not have been a parameter in the
    // first place, and behind a reference or a `Box` it is a field like any other. A named lifetime
    // needs none either, since the seed carries the parameter that declares it.
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

/// Does this type name an `impl Trait`, and does it name `Self`?
///
/// Asked of the syntax rather than of the rendered text, so that a type of the user's whose name
/// merely *contains* `Self`, such as `MySelf`, is not mistaken for it.
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

/// The generic parameters and where-predicates a lifted group's seed and machine carry: the union
/// of its members', keyed by name, since a cycle written the ordinary way declares the same ones on
/// every member and one written with a nested member declares them only on the host.
///
/// `None` when they cannot be shared, in which case the group is emitted as a copy per member:
///
/// - two members declaring the same name differently, which one list cannot satisfy;
/// - a parameter no member's *parameters* mention, which the seed enum cannot declare — an enum
///   may not have a parameter its variants never use (`E0392`). A type appearing only in a return
///   type is the usual way to hit this.
fn shared_generics(funcs: &[ItemFn]) -> Option<(Vec<syn::GenericParam>, Vec<syn::WherePredicate>)> {
    /// What a member asks of one parameter: the bounds as a *set*, so that two members asking the
    /// same thing agree however they spelled it — `T: Copy + Into<u64>` is `T: Into<u64> + Copy`,
    /// and either is `T` with `where T: Copy + Into<u64>`.
    type Asked = std::collections::BTreeMap<String, std::collections::BTreeSet<String>>;

    fn name(param: &syn::GenericParam) -> String {
        match param {
            syn::GenericParam::Lifetime(l) => format!("'{}", l.lifetime.ident),
            syn::GenericParam::Type(t) => t.ident.to_string(),
            syn::GenericParam::Const(c) => format!("const {}", c.ident),
        }
    }

    /// The name a predicate bounds, when it bounds a parameter rather than some type built from
    /// one: `where T: Copy` belongs to `T`, `where Vec<T>: Clone` belongs to nobody.
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

    /// Every parameter this member declares, with the bounds it asks of it from both places they
    /// can be written. A const parameter's type is one of its "bounds", since it has to agree too.
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

    // Each parameter is taken from the first member that declares it, together with that member's
    // own predicates about it — one spelling of the requirement, which every other member declaring
    // the same parameter has to match as a set.
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
        // A predicate about something built from a parameter belongs to no parameter, so it is
        // carried as written, once.
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

    // Every parameter has to be used by some variant of the seed, which carries the members'
    // parameters and nothing else.
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

/// A parameter type as the seed carries it./// A parameter type as the seed carries it./// A parameter type as the seed carries it.
///
/// Two rewrites. Every elided lifetime becomes the seed's own, since an enum field cannot
/// elide one. And `Self` becomes the type it stands for, since the seed is declared beside
/// the impl block rather than inside it — which is why an impl group has to supply that
/// type to be lifted at all.
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
    /// One arm per entry point and per resume point.
    arms: &'a [TokenStream],
    /// How each context slot is filled from the member's parameters.
    ctx_inits: &'a [TokenStream],
    /// `: R`, naming the driver's result type.
    ret_ann: &'a TokenStream,
    /// The union of the members' return types, when they differ. It is named by the
    /// shared machine's own signature, so it cannot live inside it.
    ret_union_decl: &'a TokenStream,
}

/// One machine for the whole group, with each member reduced to a seeded call.
///
/// The seed enum is what makes this possible: it carries only the members' own parameters,
/// whose types their signatures give, so the shared function's signature can be written. The
/// entry and frame enums stay inside that function, where their payloads are still inferred.
/// See [`liftable`] for when a group qualifies.
///
/// A member marked `inner` came out of a body, so its own entry is written *inside* the machine
/// rather than beside it, and the slot it would have taken comes back empty.
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
        ctx_inits,
        ret_ann,
        ret_union_decl,
    } = pieces;
    let members: Vec<Ident> = funcs.iter().map(|f| f.sig.ident.clone()).collect();
    let (seed_ty, machine, ctxp) = (seed_ty(&members), machine_fn(&members), ctx_param());
    let (entry, drive, lt) = (entry_ty(), drive_fn(), seed_lifetime());
    // The shared machine answers with whatever the driver answers with, which is the
    // union when the members' return types differ.
    let ret = {
        let ann = ctx.ret_ann.clone();
        let ty = ann.into_iter().skip(1).collect::<TokenStream>();
        quote! { -> #ty }
    };

    // One variant per member, holding that member's parameters as written.
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

    // The members' own generic parameters, which the seed and the machine both carry, plus the
    // seed's own lifetime — left out when no member takes a reference, since an unused lifetime
    // parameter is an error. Declared with their bounds, used without: `<'__ss, T: Copy>` names
    // them, `<'__ss, T>` passes them on.
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

    // Taking a seed apart gives back that member's parameters under their own names, so
    // the context tuple and the entry payload are built exactly as they are per member.
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

    // Each member keeps its signature and seeds its own entry. A method keeps the two levels it
    // already had: the method itself, and the plain function it forwards to, whose body is now
    // one seeded call rather than a machine of its own.
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
            // Written inside the machine, where `Self` belongs to no item: a nested `fn` cannot
            // name it (E0401), so the impl's own type is spelled out instead.
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

    // A member declared inside another's body goes into the driver rather than beside it,
    // where the items those bodies declared are, and where a helper that called it still
    // finds it under the name it had.
    let entries: Vec<TokenStream> = entries.collect();
    let within: Vec<&TokenStream> = entries
        .iter()
        .zip(inner)
        .filter(|&(_, &nested)| nested)
        .map(|(w, _)| w)
        .collect();

    let arms = arms.to_vec();
    let seed_decl = quote! {
        #ret_union_decl

        // Named after a function, hence not camel case; it is generated and not meant
        // to be written.
        #[allow(non_camel_case_types)]
        enum #seed_ty #seed_generics #where_clause {
            #(#variants,)*
        }
    };
    let machine_decl = quote! {
        #allows
        fn #machine #seed_generics (__ss_seed: #seed_ty #seed_args) #ret #where_clause {
            #machinery
            #(#within)*

            let (mut #ctxp, __ss_entry) = match __ss_seed {
                #(#dispatch)*
            };
            let __ss_out #ret_ann = #drive(
                &mut #ctxp,
                __ss_entry,
                |#ctxp, __ss_input| match __ss_input { #(#arms)* },
            );
            __ss_out
        }
    };

    // An enum cannot be declared inside an impl block, so for a group of methods the seed
    // is hoisted beside the impl and only the machine stays in it, as an associated
    // function — which is also what keeps `Self` working in the arms.
    let (hoisted, with_first) = match self_ty {
        Some(_) => (seed_decl, machine_decl),
        None => (TokenStream::new(), quote! { #seed_decl #machine_decl }),
    };

    // The declarations ride with the first member written beside the driver rather than inside
    // it — slot 0 today, but saying which one it is keeps that from being load-bearing.
    let beside = inner
        .iter()
        .position(|&nested| !nested)
        .expect("a cycle has a member written beside its driver");
    let out: Vec<TokenStream> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            if inner[i] {
                // Already written inside the driver.
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

/// The name an item declares, where it declares a single one.
///
/// A `use`, an `impl` and a macro-generated item declare no name this can compare, so they
/// are gathered without being checked; a genuine clash between two of those is then Rust's
/// own duplicate-definition error rather than the macro's.
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

/// A type as the user would write it, for error messages: `to_token_stream`
/// renders `&mut Vec<u64>` as `& mut Vec < u64 >`.
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

/// The driver's result type is exactly the members' return type. Naming it —
/// on the driver's `let` and on every continuation's parameter — is what lets
/// method resolution inside a continuation see its receiver's type; left to
/// inference, `f(n - 1).wrapping_add(1)` fails with E0689 ("ambiguous numeric
/// type"). `impl Trait` is the one return type that cannot be written down, so it
/// goes unannotated.
fn ret_annotation(sig: &syn::Signature) -> TokenStream {
    match &sig.output {
        ReturnType::Default => quote! { : () },
        ReturnType::Type(_, ty) if !matches!(&**ty, syn::Type::ImplTrait(_)) => quote! { : #ty },
        ReturnType::Type(..) => quote! {},
    }
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

/// Transform a group of functions that share one driver: a self-recursive
/// function alone, or every member of a mutually recursive cycle. Emits one
/// rewritten function per member, each seeded at its own entry point.
/// What the whole group shares, worked out before a line of it is generated: how each member's
/// parameters split into a payload and context slots, what the members have to agree on, and what
/// the driver answers with. The body scans run here too, since everything the arms are built from
/// depends on their answers.
fn analyse(funcs: &[ItemFn], opts: Opts, assoc: bool) -> syn::Result<Ctx> {
    let mut splits = Vec::new();
    for func in funcs {
        reject_unsupported_signature(&func.sig)?;
        splits.push(split_params(func)?);
    }

    // The whole group shares one context tuple and one result type, so its
    // members have to agree on both.
    let first = &splits[0];
    for (split, func) in splits.iter().zip(funcs).skip(1) {
        if split.slot_types != first.slot_types {
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

    // The driver has one result type. Members that answer with different types answer
    // with a union of them instead, which each member's entry takes its own variant out of.
    // `impl Trait` cannot be named, so such a return goes unannotated and unjoined.
    let rets: Vec<TokenStream> = funcs.iter().map(|f| ret_annotation(&f.sig)).collect();
    let differ = rets.iter().any(|r| r.to_string() != rets[0].to_string());
    // An `impl Trait` return is its own opaque type, so two members that spell one
    // identically still return *different* types, and neither one can be named to join
    // them. A lone function is unaffected: it is the only thing the driver answers for.
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
        ret_union: ret_union.clone(),
        opts,
    };

    for func in funcs {
        validate(&ctx, &func.block)?;
        // Must run before any code is generated: it decides which slots are raw,
        // which every context rebinding depends on.
        scan_context_args(&ctx, &func.block)?;
        // Likewise: it decides which payload positions travel as a raw pointer into
        // the driver's pinned store.
        scan_pinned_args(&ctx, &func.block)?;
    }
    Ok(ctx)
}

/// Each member's body, turned into the arms it is entered at, and the items those bodies declared.
///
/// A body is split across several arms of the shared `match`, so an item it declares is moved out to
/// one place enclosing all of them, and that place serves the whole group. Two members declaring the
/// same name would therefore declare it twice, which is the one thing to reject.
fn member_arms(ctx: &Ctx, funcs: &[ItemFn]) -> syn::Result<(Vec<Item>, Vec<TokenStream>)> {
    // A body is split across several arms of the shared `match`, so an item it declares is
    // moved out to one place enclosing all of them — and that place serves the whole group.
    // Two members declaring the same name would therefore declare it twice, which is the one
    // thing to reject: `declared` records who declared what.
    let mut declared: HashMap<String, &Ident> = HashMap::new();
    let mut items: Vec<Item> = Vec::new();
    let mut main_arms: Vec<TokenStream> = Vec::new();
    let step = step_ty();
    let entry = entry_ty();

    let input = input_ty();

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
        };
        // Each member's own result enters the union under its own variant.
        let done = |v: TokenStream| -> syn::Result<TokenStream> {
            let v = ctx.wrap_result(i, v);
            Ok(quote! { #step::Done(#v) })
        };
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
            #input::Enter(#entry::#variant((#(#pats,)*))) => { #prologue #(#anns)* #arm },
        });
    }

    Ok((items, main_arms))
}

/// `inner[i]` says that member `i` was declared inside another member's body. It keeps the
/// name it had — a body's name is nobody else's — but is written *inside* the shared driver,
/// beside the items those bodies declared, rather than out at the group's own scope, where it
/// was never visible in the first place.
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

    // `self` is special only to Rust's syntax, so it is desugared away first: a method
    // becomes a plain function of an ordinary `&Self` or `&mut Self` parameter, plus a
    // wrapper that keeps the method's own signature. Everything below then deals with
    // functions only, and a receiver obeys whatever rule its type already implies.
    let group_names: Vec<Ident> = funcs.iter().map(|f| f.sig.ident.clone()).collect();
    let mut methods: Vec<Option<MethodSplit>> = Vec::with_capacity(funcs.len());
    for func in &mut funcs {
        methods.push(desugar_receiver(func, &group_names)?);
    }

    // Decided as soon as receivers are out of the way, since everything below depends on it and
    // one of the two shapes cannot hold every group.
    let lift = liftable(&funcs, self_ty.is_some());
    // A shared driver that is generic is no home for a member declared in a body: such a function
    // cannot name the parameters — a nested `fn` never sees the generics of the one hosting it — so
    // it could only call the cycle at some concrete type, which is not what the driver is.
    let generic = lift && shared_generics(&funcs).is_some_and(|(params, _)| !params.is_empty());
    if (!lift || generic)
        && let Some((f, _)) = funcs.iter().zip(inner).find(|&(_, &nested)| nested)
    {
        // The unlifted shape gives each member its own copy of the machinery, and a member written
        // inside the machinery cannot hold a copy containing itself.
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

    let ctx = analyse(&funcs, opts, assoc)?;

    let (items, main_arms) = member_arms(&ctx, &funcs)?;
    let (entry, input, drive) = (entry_ty(), input_ty(), drive_fn());

    // Resolve every payload — loop states and resume frames together, since they
    // reference each other's markers.
    let mut ctx_inits: Vec<TokenStream> = ctx.context.iter().map(CtxEntry::init_expr).collect();
    let pin = pin_ty();
    for elem in ctx.pin_elements() {
        // Naming the element type here, rather than leaving it to the `push`, is what
        // lets one member's entry payload be inferred from another's arm.
        ctx_inits.push(match elem {
            Some(elem) => quote! { #pin::<#elem>::new() },
            None => quote! { #pin::new() },
        });
    }
    let loop_base = ctx.loop_base();
    let loops = ctx.loops.take();
    let resumes = ctx.resumes.take();
    let (states, frames) = solve_payloads(&loops, &resumes);

    let mut subst = HashMap::new();
    for (n, st) in states.iter().enumerate() {
        subst.insert(state_marker(n).to_string(), quote! { #(#st,)* });
    }
    for (r, fr) in frames.iter().enumerate() {
        subst.insert(frame_marker(r).to_string(), quote! { #(#fr,)* });
    }

    let frame = frame_ty();

    let mut arms = main_arms;
    for (n, lp) in loops.iter().enumerate() {
        let v = entry_variant(loop_base + n);
        let st = &states[n];
        let code = &lp.code;
        let prologue = ctx.ctx_prologue();
        arms.push(quote! {
            #input::Enter(#entry::#v((#(mut #st,)*))) => { #prologue #code },
        });
    }
    // One arm per recursive call site: where the driver resumes with the result.
    for (r, res) in resumes.iter().enumerate() {
        let variant = frame_variant(r);
        let payload = &frames[r];
        let value = &res.value;
        let code = &res.point.code;
        arms.push(quote! {
            #input::Resume(#frame::#variant((#(mut #payload,)*)), #value) => { #code },
        });
    }
    // With no recursive call there is no frame, so the enum is uninhabited and the
    // arm is proved unreachable rather than written.
    if resumes.is_empty() {
        arms.push(quote! { #input::Resume(__ss_never, _) => match __ss_never {}, });
    }
    let arms: Vec<TokenStream> = arms.into_iter().map(|ts| substitute(ts, &subst)).collect();

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

    let defs_imports = defs_imports();
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

        enum #entry<#(#entry_params),*> {
            #(#entry_variants(#entry_params),)*
        }

        enum #frame<#(#frame_params),*> {
            #(#frame_variants(#frame_params),)*
        }
    };
    // The expansion legitimately produces `mut` bindings a given arm does not use,
    // redundant parens, and arms after a `return`. `unused_assignments` fires falsely on
    // a loop-carried local: the continuation assigns it and then moves it into the next
    // iteration's payload, which the upvar analysis does not count as a read.
    let allows = quote! {
        #[allow(
            unused_mut,
            unused_variables,
            unused_parens,
            unused_assignments,
            unreachable_code,
            // `break` inside a lowered loop becomes `return Done(..)`, which can land in
            // a sub-expression position.
            clippy::diverging_sub_expression
        )]
    };

    if lift {
        let pieces = Pieces {
            machinery: &machinery,
            allows: &allows,
            arms: &arms,
            ctx_inits: &ctx_inits,
            ret_ann: &ctx.ret_ann,
            ret_union_decl: &ret_union_decl,
        };
        return lifted(&funcs, &ctx, &pieces, self_ty, &methods, inner);
    }

    // One rewritten function per member, each with its own copy of the machinery. See
    // `liftable` for why a group sometimes has to be emitted this way, and the rejection above
    // for why no member of such a group can have come out of a body.
    let ret_ann = &ctx.ret_ann;
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

        // A method keeps its own signature in the wrapper, and the transformed body is
        // emitted beside it under the wrapper's chosen name.
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
            // The expansion legitimately produces `mut` bindings a given arm does
            // not use, redundant parens, and arms after a `return`. `unused_assignments`
            // fires falsely on a loop-carried local: the continuation assigns it and
            // then moves it into the next iteration's payload, which the upvar
            // analysis does not count as a read.
            #[allow(
                unused_mut,
                unused_variables,
                unused_parens,
                unused_assignments,
                unreachable_code,
                // `break` inside a lowered loop becomes `return Done(..)`, which can
                // land in a sub-expression position.
                clippy::diverging_sub_expression
            )]
            #vis #sig {
                #defs_imports
                #(#items)*

                #ret_union_decl

                enum #entry<#(#entry_params),*> {
                    #(#entry_variants(#entry_params),)*
                }

                enum #frame<#(#frame_params),*> {
                    #(#frame_variants(#frame_params),)*
                }

                let mut #ctxp = (#(#ctx_inits,)*);
                let __ss_out #ret_ann = #drive(
                    &mut #ctxp,
                    #entry::#variant((#(#seed,)*)),
                    |#ctxp, __ss_input| match __ss_input { #(#arms)* },
                );
                #take_out
            }
        });
    }
    Ok((out, TokenStream::new()))
}
