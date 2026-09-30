// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! State carried by the transform: the per-group [`Ctx`], the per-position [`Env`], and
//! the continuation type [`Cont`].

use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use super::names::{entry_variant, try_trait};
use syn::Expr;

use super::Opts;
use super::context::CtxEntry;

/// A point the driver can arrive at: a lowered loop's entry, or a resume point after a
/// recursive call. Its payload is solved to a fixed point.
pub(super) struct PayloadPoint {
    /// Member whose body this point is in (one name can have different types in two bodies).
    pub(super) member: usize,
    /// Bindings in scope, in declaration order; payload order must be stable.
    pub(super) scope: Vec<Ident>,
    /// Always threaded, first: a `for` iterator, and parked `use_nonlinear_mut` context pointers.
    pub(super) forced: Vec<Ident>,
    /// The code, still containing payload markers.
    pub(super) code: TokenStream,
    /// `#[cfg]` predicates enclosing this point, outermost first; its arm is gated on them.
    pub(super) gates: Vec<TokenStream>,
    /// Bindings in scope that the arm recomputes instead of carrying.
    pub(super) derived: Vec<Derived>,
}

/// A binding recomputed from another threaded binding instead of being carried.
///
/// The only case is the index of a `for` over `a..b`, read back from the range's `start`.
/// See `cps::lower_loop`.
#[derive(Clone)]
pub(super) struct Derived {
    /// The binding, as the user's pattern spells it.
    pub(super) name: Ident,
    /// What it is recomputed from, which the payload carries in its place.
    pub(super) from: Ident,
    /// The expression recomputing it, in terms of `from`.
    pub(super) expr: TokenStream,
}

/// A resume point: where execution continues once a callee returns.
pub(super) struct ResumePoint {
    pub(super) point: PayloadPoint,
    /// The binding the callee's result arrives in.
    pub(super) value: Ident,
    /// Whether this point's `?` could share one hoisted check. See `Ctx::mark_checked`.
    pub(super) hoistable: Cell<bool>,
    /// The `?` was hoisted: [`ResumePoint::value`] is already checked. See `driver::resume`.
    pub(super) checked: Cell<bool>,
}

/// A function the driver can enter. A group of one or more members shares one driver.
#[derive(Clone)]
pub(super) struct Member {
    pub(super) name: Ident,
    /// Number of arguments, receiver included (it is desugared into a parameter).
    pub(super) arity: usize,
    /// Argument position -> context slot. Per member: members agree on slots, not positions.
    pub(super) context_at: HashMap<usize, usize>,
    /// Payload parameters, as patterns (`mut n`) and as names (`n`).
    pub(super) param_pats: Vec<TokenStream>,
    pub(super) param_names: Vec<Ident>,
    /// `let mut n: u64 = n;` per payload parameter, at the top of the member's arm.
    ///
    /// Pins the payload type: in a group, match ergonomics on a reference parameter could
    /// otherwise infer the by-value type.
    pub(super) param_anns: Vec<TokenStream>,
    /// Same, for a parameter passed as a raw pointer into the pinned store.
    pub(super) param_anns_pinned: Vec<TokenStream>,
    /// Which payload positions are pinned. Set later by `analyze::scan_pinned_args`.
    pub(super) pinned: Vec<std::cell::Cell<bool>>,
    /// Pointee type of a reference parameter: the element type of its store.
    pub(super) param_pointees: Vec<Option<TokenStream>>,
    /// `: u64` per payload parameter, empty for `impl Trait`.
    pub(super) param_types: Vec<TokenStream>,
    /// The same types without the colon.
    pub(super) param_bare_types: Vec<TokenStream>,
}

pub(super) struct Ctx {
    /// Every function sharing this driver. Member `i` enters at `E{i}`; loops come after.
    pub(super) members: Vec<Member>,
    pub(super) counter: Cell<usize>,
    pub(super) loops: RefCell<Vec<PayloadPoint>>,
    /// One per recursive call site: a frame variant plus a `match` arm.
    pub(super) resumes: RefCell<Vec<ResumePoint>>,
    /// Result bindings of the continuations being generated: in scope, but not in any `Env`.
    pub(super) results: RefCell<Vec<Ident>>,
    /// Parameters lent out by the driver instead of carried in the payload, in slot order.
    pub(super) context: Vec<CtxEntry>,
    /// Members are associated items, so `self::g(..)` is not a member call. See `scope::edges`.
    pub(super) assoc: bool,
    /// `: R` for the driver's result: the union, when the members' return types differ.
    pub(super) ret_ann: TokenStream,
    /// Each member's return type as `: R` (empty for `impl Trait`), used to annotate resumed
    /// values so method calls on them resolve.
    pub(super) rets: Vec<TokenStream>,
    /// The same, as bare types, to name payload slots that inference cannot see (e.g. when
    /// the only construction is `#[cfg]`-ed out).
    pub(super) ret_types: Vec<TokenStream>,
    /// Union of the members' return types, when they differ.
    pub(super) ret_union: Option<Ident>,
    pub(super) opts: Opts,
    /// Member whose body is being lowered.
    pub(super) current: Cell<usize>,
    /// `#[cfg]` predicates around the code being lowered, outermost first.
    pub(super) gates: RefCell<Vec<TokenStream>>,
    /// Declared type of each annotated `let`, by `(member, name)`; `None` if inconsistent.
    pub(super) local_types: RefCell<HashMap<(usize, String), Option<TokenStream>>>,
    /// Every name a member binds with a plain `let`, whether or not that `let` said a type.
    pub(super) locals: RefCell<HashSet<(usize, String)>>,
    /// Shared-store variants requested during lowering, in request order: one per borrowing
    /// `for` collection, one per parked local.
    pub(super) asked_stores: RefCell<Vec<(StoreKey, TokenStream)>>,
    /// Hoist every resume point's `?` into one check above the frame dispatch? All or
    /// nothing; decided up front by `emit::checks_are_shareable`.
    pub(super) hoist: Cell<bool>,
}

impl Ctx {
    /// The entry-variant index of the first lowered loop.
    pub(super) fn loop_base(&self) -> usize {
        self.members.len()
    }

    pub(super) fn member(&self, i: usize) -> &Member {
        &self.members[i]
    }

    /// Run `f` with `v` recorded as a live result binding.
    pub(super) fn with_result<R>(&self, v: Ident, f: impl FnOnce() -> R) -> R {
        self.results.borrow_mut().push(v);
        let out = f();
        self.results.borrow_mut().pop();
        out
    }

    /// User bindings in scope, plus result bindings of the enclosing continuations.
    pub(super) fn scope_with_results(&self, scope: &[Ident]) -> Vec<Ident> {
        let mut out = scope.to_vec();
        for v in self.results.borrow().iter() {
            if !out.contains(v) {
                out.push(v.clone());
            }
        }
        out
    }

    pub(super) fn fresh(&self) -> Ident {
        let n = self.counter.get();
        self.counter.set(n + 1);
        format_ident!("__ss_v{}", n)
    }

    /// Every pinned payload position, in a fixed order, as `(member, position)`.
    fn pinned_positions(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (i, p) in self.members.iter().enumerate() {
            for (j, cell) in p.pinned.iter().enumerate() {
                if cell.get() {
                    out.push((i, j));
                }
            }
        }
        out
    }

    /// Element type of a store: the pointee of that position's parameter.
    fn pin_element(&self, member: usize, position: usize) -> Option<TokenStream> {
        self.members[member].param_pointees[position].clone()
    }

    /// Pinned positions whose element type is known, in variant order.
    fn named_positions(&self) -> Vec<(usize, usize)> {
        self.pinned_positions()
            .into_iter()
            .filter(|&(i, j)| self.pin_element(i, j).is_some())
            .collect()
    }

    /// Pinned positions whose element type is unknown; each gets its own store.
    fn unnamed_positions(&self) -> Vec<(usize, usize)> {
        self.pinned_positions()
            .into_iter()
            .filter(|&(i, j)| self.pin_element(i, j).is_none())
            .collect()
    }

    /// Context-tuple index of the shared store, after the own stores.
    fn shared_slot(&self) -> syn::Index {
        syn::Index::from(self.context.len() + self.unnamed_positions().len())
    }

    /// Number of own stores.
    pub(super) fn own_store_count(&self) -> usize {
        self.unnamed_positions().len()
    }

    /// Shared store element types in variant order: known pinned positions, then requested
    /// stores. Empty means no shared store is emitted.
    pub(super) fn shared_elements(&self) -> Vec<TokenStream> {
        let mut out: Vec<TokenStream> = self
            .named_positions()
            .iter()
            .map(|&(i, j)| self.pin_element(i, j).expect("nameable"))
            .collect();
        out.extend(self.asked_stores.borrow().iter().map(|(_, e)| e.clone()));
        out
    }

    /// Where a value lent to a call is stored.
    pub(super) fn held_pin(&self, member: usize, position: usize) -> Held {
        match self.pin_element(member, position) {
            Some(_) => Held::Shared {
                slot: self.shared_slot(),
                variant: self
                    .named_positions()
                    .iter()
                    .position(|&p| p == (member, position))
                    .expect("only called for a pinned position"),
            },
            None => Held::Own {
                slot: syn::Index::from(
                    self.context.len()
                        + self
                            .unnamed_positions()
                            .iter()
                            .position(|&p| p == (member, position))
                            .expect("only called for a pinned position"),
                ),
            },
        }
    }

    /// Reserve a shared-store variant for a borrowing loop's collection.
    pub(super) fn held_loop(&self, loop_idx: usize, elem: TokenStream) -> Held {
        self.held_asked(StoreKey::Loop(loop_idx), elem)
    }

    /// Reserve the shared-store variant a call site parks `root` in.
    ///
    /// Keyed by local, not call site: a site may be lowered twice (under a `#[cfg]` and its
    /// negation). Sharing is safe because the store is a stack.
    pub(super) fn held_root(&self, root: &Ident) -> Held {
        let member = self.current.get();
        let elem = self
            .slot_type(member, root)
            .expect("a place lend requires an annotated local; see `owns_annotated_local`");
        self.held_asked(StoreKey::Root(member, root.to_string()), elem)
    }

    /// The shared store's variant for one asked-for entity, reserving it on first ask.
    fn held_asked(&self, key: StoreKey, elem: TokenStream) -> Held {
        let slot = self.shared_slot();
        let named = self.named_positions().len();
        let mut stores = self.asked_stores.borrow_mut();
        let at = match stores.iter().position(|(k, _)| *k == key) {
            Some(at) => at,
            None => {
                stores.push((key, elem));
                stores.len() - 1
            }
        };
        Held::Shared {
            slot,
            variant: named + at,
        }
    }

    /// Record that the member being lowered binds `name` with a `let`.
    pub(super) fn note_local(&self, name: &Ident) {
        self.locals
            .borrow_mut()
            .insert((self.current.get(), name.to_string()));
    }

    /// Record an annotated `let` binding of the member being lowered.
    pub(super) fn note_local_type(&self, name: &Ident, ty: TokenStream) {
        let key = (self.current.get(), name.to_string());
        let rendered = ty.to_string();
        let mut map = self.local_types.borrow_mut();
        match map.get(&key) {
            // Shadowed with a different type: no single annotation fits.
            Some(Some(seen)) if seen.to_string() != rendered => {
                map.insert(key, None);
            }
            Some(_) => {}
            None => {
                map.insert(key, Some(ty));
            }
        }
    }

    /// Whether `e` names an annotated, non-reference local of `member`.
    ///
    /// Required before lending a place inside a local (which parks it), so that e.g.
    /// `let args = &node.args; f(&args[i])` stays a plain borrow.
    pub(super) fn owns_annotated_local(&self, member: usize, e: &syn::Expr) -> bool {
        let syn::Expr::Path(p) = e else { return false };
        let Some(name) = p.path.get_ident() else {
            return false;
        };
        if self.param_type_of(member, name).is_some() {
            return false;
        }
        match self
            .local_types
            .borrow()
            .get(&(member, name.to_string()))
            .cloned()
            .flatten()
        {
            Some(ty) => !matches!(syn::parse2::<syn::Type>(ty), Ok(syn::Type::Reference(_))),
            None => false,
        }
    }

    /// Whether `e` names a `let` binding of `member` that is not an annotated reference, so
    /// lending it needs the store.
    pub(super) fn owns_named_local(&self, member: usize, e: &syn::Expr) -> bool {
        let syn::Expr::Path(p) = e else { return false };
        let Some(name) = p.path.get_ident() else {
            return false;
        };
        if self.param_type_of(member, name).is_some() {
            return false;
        }
        let annotated_reference = self
            .local_types
            .borrow()
            .get(&(member, name.to_string()))
            .cloned()
            .flatten()
            .is_some_and(|ty| matches!(syn::parse2::<syn::Type>(ty), Ok(syn::Type::Reference(_))));
        !annotated_reference && self.locals.borrow().contains(&(member, name.to_string()))
    }

    /// A payload slot's type, from the signature or an annotated `let`.
    pub(super) fn slot_type(&self, member: usize, name: &Ident) -> Option<TokenStream> {
        self.param_type_of(member, name).or_else(|| {
            self.local_types
                .borrow()
                .get(&(member, name.to_string()))
                .cloned()
                .flatten()
        })
    }

    /// Declared type of a payload parameter, unless `impl Trait`. For a pinned position this
    /// is the reference type, not the pointer.
    pub(super) fn param_type_of(&self, member: usize, name: &Ident) -> Option<TokenStream> {
        let member = &self.members[member];
        let j = member.param_names.iter().position(|p| p == name)?;
        let bare = member.param_bare_types.get(j)?;
        (!bare.is_empty()).then(|| bare.clone())
    }

    /// The declared type of a payload parameter of the member being lowered.
    pub(super) fn current_param_type(&self, name: &Ident) -> Option<TokenStream> {
        self.param_type_of(self.current.get(), name)
    }

    /// Context rebindings in slot order, emitted at the top of the body and every continuation.
    pub(super) fn ctx_prologue(&self) -> TokenStream {
        let binds = self.context.iter().enumerate().map(|(i, e)| e.rebind(i));
        quote! { #(#binds)* }
    }

    /// True if [`Self::ctx_prologue`] only rebinds names (no raw pointers), so an unused one can
    /// be dropped.
    pub(super) fn ctx_prologue_only_rebinds(&self) -> bool {
        self.context.iter().all(|e| !e.raw.get())
    }

    /// Reserve an entry point for a loop; the code is filled in afterwards.
    pub(super) fn reserve_loop(
        &self,
        scope: Vec<Ident>,
        derived: Vec<Derived>,
        iter: Option<Ident>,
        also_forced: Vec<Ident>,
    ) -> usize {
        let mut loops = self.loops.borrow_mut();
        loops.push(PayloadPoint {
            member: self.current.get(),
            scope,
            forced: iter.into_iter().chain(also_forced).collect(),
            code: TokenStream::new(),
            gates: self.gates.borrow().clone(),
            derived,
        });
        loops.len() - 1
    }

    pub(super) fn set_loop_body(&self, idx: usize, body: TokenStream) {
        self.loops.borrow_mut()[idx].code = body;
    }

    /// Bind a resumed value, taking it out of the union if there is one.
    pub(super) fn unwrap_result(&self, callee: usize, v: &Ident) -> TokenStream {
        let bare = &self.ret_types[callee];
        if !bare.is_empty() {
            self.note_local_type(v, bare.clone());
        }
        let Some(union) = &self.ret_union else {
            let ann = &self.rets[callee];
            return quote! { let #v #ann = #v; };
        };
        let variant = entry_variant(callee);
        let ann = &self.rets[callee];
        quote! {
            let #v #ann = match #v {
                #union::#variant(__ss_r) => __ss_r,
                // A call to one member answers with that member's variant.
                _ => ::core::unreachable!("stack_safe: result of the wrong member"),
            };
        }
    }

    /// How a member's entry takes its own result back out of the union.
    pub(super) fn take_result(&self, member: usize) -> TokenStream {
        match &self.ret_union {
            None => quote! { __ss_out },
            Some(union) => {
                let variant = entry_variant(member);
                quote! {
                    match __ss_out {
                        #union::#variant(__ss_r) => __ss_r,
                        // The driver was seeded at this member, so it answers for it.
                        _ => ::core::unreachable!("stack_safe: result of the wrong member"),
                    }
                }
            }
        }
    }

    /// How a member's own result enters the union, if there is one.
    pub(super) fn wrap_result(&self, member: usize, v: TokenStream) -> TokenStream {
        match &self.ret_union {
            None => v,
            Some(union) => {
                let variant = entry_variant(member);
                quote! { #union::#variant(#v) }
            }
        }
    }

    /// Type of `e` if it is a bare binding with a known type; nothing else is guessed.
    pub(super) fn type_of(&self, e: &syn::Expr) -> Option<TokenStream> {
        let syn::Expr::Path(p) = e else { return None };
        let name = p.path.get_ident()?;
        self.slot_type(self.current.get(), name)
    }

    /// Lower `body` as written under one more `#[cfg]` predicate.
    pub(super) fn under_gate<T>(
        &self,
        gate: TokenStream,
        body: impl FnOnce() -> syn::Result<T>,
    ) -> syn::Result<T> {
        self.gates.borrow_mut().push(gate);
        let out = body();
        self.gates.borrow_mut().pop();
        out
    }

    /// Reserve a resume point; its code is filled in later.
    ///
    /// `hoistable`: nothing needs tearing down before its code, so its `?` may be hoisted.
    pub(super) fn reserve_resume(
        &self,
        scope: Vec<Ident>,
        derived: Vec<Derived>,
        forced: Vec<Ident>,
        value: Ident,
        hoistable: bool,
    ) -> usize {
        let mut resumes = self.resumes.borrow_mut();
        resumes.push(ResumePoint {
            point: PayloadPoint {
                member: self.current.get(),
                scope,
                forced,
                code: TokenStream::new(),
                gates: self.gates.borrow().clone(),
                derived,
            },
            value,
            hoistable: Cell::new(hoistable),
            checked: Cell::new(false),
        });
        resumes.len() - 1
    }

    /// If the resume point still being generated for `v` is hoistable, mark its `?` hoisted
    /// and return `true`.
    pub(super) fn mark_checked(&self, v: &TokenStream) -> bool {
        let name = v.to_string();
        let resumes = self.resumes.borrow();
        let Some(point) = resumes
            .iter()
            .find(|p| p.point.code.is_empty() && p.value == name)
        else {
            return false;
        };
        point.hoistable.get() && {
            point.checked.set(true);
            true
        }
    }

    /// Record a hoisted point's value as the carrier's `Output` type.
    pub(super) fn note_unwrapped(&self, callee: usize, v: &Ident) {
        let bare = &self.ret_types[callee];
        if bare.is_empty() {
            return;
        }
        let tr = try_trait();
        self.note_local_type(v, quote! { <#bare as #tr>::Output });
    }

    pub(super) fn is_checked(&self, idx: usize) -> bool {
        self.resumes.borrow()[idx].checked.get()
    }

    pub(super) fn set_resume_code(&self, idx: usize, code: TokenStream) {
        self.resumes.borrow_mut()[idx].point.code = code;
    }

    /// Drop the last reserved resume point, for a tail call. Returns `false` unless `idx` is
    /// last and has no code yet.
    pub(super) fn drop_last_resume(&self, idx: usize) -> bool {
        let mut resumes = self.resumes.borrow_mut();
        if resumes.len() != idx + 1 || !resumes[idx].point.code.is_empty() {
            return false;
        }
        resumes.pop();
        true
    }

    /// Callee index if `e` calls a member. Receivers are already desugared, so
    /// `self.walk(a)` is `walk(a)` here.
    pub(super) fn rec_call<'e>(&self, e: &'e Expr) -> Option<(usize, &'e syn::ExprCall)> {
        let Expr::Call(call) = e else { return None };
        let Expr::Path(p) = &*call.func else {
            return None;
        };
        let segments = &p.path.segments;
        // `self::g(..)` names a member only when the members are module functions.
        let named = match segments.len() {
            1 => true,
            2 => !self.assoc && segments[0].ident == "self",
            _ => false,
        };
        if p.qself.is_some() || !named {
            return None;
        }
        self.index_of(&segments.last().expect("non-empty path").ident)
            .map(|callee| (callee, call))
    }

    pub(super) fn is_rec_call(&self, e: &Expr) -> bool {
        self.rec_call(e).is_some()
    }

    pub(super) fn index_of(&self, name: &Ident) -> Option<usize> {
        self.members.iter().position(|p| &p.name == name)
    }

    /// The members' names, for error messages.
    pub(super) fn names(&self) -> Vec<&Ident> {
        self.members.iter().map(|p| &p.name).collect()
    }
}

/// A macro-level continuation: given tokens for the value produced at this point, produce
/// the tokens for the rest of the body.
pub(super) type Cont<'a> = &'a dyn Fn(TokenStream) -> syn::Result<TokenStream>;

/// The innermost lowered loop, for rewriting `break` / `continue`.
pub(super) struct LoopCtx<'a> {
    /// Index among lowered loops; names its state placeholder.
    pub(super) idx: usize,
    /// Entry variant index (members come first); `continue` tail-enters it.
    pub(super) variant: usize,
    /// `break` runs the code that follows the loop.
    pub(super) brk: Cont<'a>,
    /// Advances the iterator before re-entering; empty if the loop head does it.
    pub(super) advance: TokenStream,
}

/// What the transform needs to know at each point in the walk.
#[derive(Clone)]
pub(super) struct Env<'a> {
    /// Bindings in scope, in declaration order.
    pub(super) scope: Vec<Ident>,
    pub(super) lp: Option<&'a LoopCtx<'a>>,
    /// Restores to run before any escape (`?`, `return`, `break`, `continue`), so a swapped
    /// context slot gets the parent's pointer back.
    pub(super) restores: TokenStream,
    /// Store truncations to run before `?` or `return` (not `continue`; `break` releases via
    /// the continuation).
    pub(super) teardown: TokenStream,
    /// Union type and this member's variant, for wrapping `return` / `?` values.
    pub(super) wrap: Option<(Ident, Ident)>,
    /// Bindings recomputable at the arrival arm; shadowing drops them.
    pub(super) derived: Vec<Derived>,
}

impl Env<'_> {
    /// Wrap `v` into the union, if there is one.
    pub(super) fn wrapped(&self, v: TokenStream) -> TokenStream {
        match &self.wrap {
            None => v,
            Some((union, variant)) => quote! { #union::#variant(#v) },
        }
    }
}

impl<'a> Env<'a> {
    pub(super) fn bind(&self, ids: impl IntoIterator<Item = Ident>) -> Env<'a> {
        let mut next = self.clone();
        for id in ids {
            next.derived.retain(|d| d.name != id);
            if !next.scope.iter().any(|i| i == &id) {
                next.scope.push(id);
            }
        }
        next
    }

    /// `name`, just bound, can be recomputed from `from` by `expr` everywhere it stays in scope.
    pub(super) fn derive(&self, name: Ident, from: Ident, expr: TokenStream) -> Env<'a> {
        let mut next = self.clone();
        next.derived.retain(|d| d.name != name);
        next.derived.push(Derived { name, from, expr });
        next
    }

    /// Entering a lowered loop replaces the `break` / `continue` target.
    pub(super) fn in_loop(&self, lp: &'a LoopCtx<'a>) -> Env<'a> {
        Env {
            scope: self.scope.clone(),
            lp: Some(lp),
            restores: self.restores.clone(),
            teardown: self.teardown.clone(),
            wrap: self.wrap.clone(),
            derived: self.derived.clone(),
        }
    }

    /// Add a truncation to run before `?` or `return`.
    pub(super) fn with_teardown(&self, extra: TokenStream) -> Env<'a> {
        let mut teardown = self.teardown.clone();
        teardown.extend(extra);
        Env {
            teardown,
            ..self.clone()
        }
    }

    pub(super) fn with_restores(&self, restores: TokenStream) -> Env<'a> {
        Env {
            restores,
            ..self.clone()
        }
    }
}

/// Where a driver-held value lives: a variant of the shared store, or its own store when
/// its type cannot be named.
#[derive(Clone)]
pub(super) enum Held {
    Shared { slot: syn::Index, variant: usize },
    Own { slot: syn::Index },
}

impl Held {
    /// The context-tuple index of the store this value lives in.
    pub(super) fn slot(&self) -> syn::Index {
        match self {
            Held::Shared { slot, .. } | Held::Own { slot } => slot.clone(),
        }
    }

    /// Which variant wraps it, if the store is the shared one.
    pub(super) fn variant(&self) -> Option<usize> {
        match self {
            Held::Shared { variant, .. } => Some(*variant),
            Held::Own { .. } => None,
        }
    }
}

/// What a store variant was reserved for, so repeated requests get the same one.
#[derive(Clone, PartialEq, Eq)]
pub(super) enum StoreKey {
    /// The collection a `for` loop borrows.
    Loop(usize),
    /// A local parked to lend a place inside it, as `(member, name)`. See [`Ctx::held_root`].
    Root(usize, String),
}
