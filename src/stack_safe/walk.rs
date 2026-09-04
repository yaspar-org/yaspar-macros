// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! What the transform carries as it walks: the per-function [`Ctx`], the
//! per-position [`Env`], and the macro-level continuation type.

use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use super::names::entry_variant;
use syn::Expr;

use super::Opts;
use super::context::CtxEntry;

/// A place the driver can arrive at, whose payload is solved to a fixed point: a
/// lowered loop's entry point, or a resume point after a recursive call.
pub(super) struct PayloadPoint {
    /// Which member's body this point came out of; one name can mean different types in two of
    /// them.
    pub(super) member: usize,
    /// Bindings in scope there, in declaration order. Threading state through a
    /// payload is a move, so the order must be stable.
    pub(super) scope: Vec<Ident>,
    /// Threaded first and unconditionally: a `for` loop's iterator, and the parked
    /// context pointers of a `use_nonlinear_mut` call.
    pub(super) forced: Vec<Ident>,
    /// The code, still containing payload markers.
    pub(super) code: TokenStream,
    /// The `#[cfg]` predicates this point was written under, outermost first. The arm generated
    /// for it exists only when they all hold; see `Ctx::gates`.
    pub(super) gates: Vec<TokenStream>,
}

/// A resume point: where execution continues once a callee returns.
pub(super) struct ResumePoint {
    pub(super) point: PayloadPoint,
    /// The binding the callee's result arrives in.
    pub(super) value: Ident,
}

/// One function the driver can enter. A self-recursive function is a group of
/// one; a mutually recursive cycle is a group of several, sharing one driver.
#[derive(Clone)]
pub(super) struct Member {
    pub(super) name: Ident,
    /// How many arguments a call to it passes. A receiver has been desugared into an
    /// ordinary parameter by this point, so it is counted like any other.
    pub(super) arity: usize,
    /// Which of those argument positions are context slots, and which slot each
    /// one fills. Members of a group agree on the *slots* but not necessarily on
    /// the positions, so this is per member.
    pub(super) context_at: HashMap<usize, usize>,
    /// Payload parameters, as patterns (`mut n`) and as names (`n`).
    pub(super) param_pats: Vec<TokenStream>,
    pub(super) param_names: Vec<Ident>,
    /// `let mut n: u64 = n;` per payload parameter, emitted at the top of this
    /// member's arm.
    ///
    /// Without it the payload's type is whatever the arm's own code implies, and a
    /// body that matches straight on a reference parameter (`match e { E::V(v) =>
    /// .. }`) implies the *by-value* type: match ergonomics only apply once the
    /// scrutinee is known to be a reference. A lone function survives that because
    /// its seed is checked before the closure, but in a group the seed for one
    /// member sits inside another member's arm, so the pattern gets there first.
    pub(super) param_anns: Vec<TokenStream>,
    /// The same rebinding for a parameter that travels as a raw pointer, because
    /// some call site passes a reference to a value built there: the pointee lives in
    /// the driver's pinned store, so the reference is taken back with `unsafe`.
    pub(super) param_anns_pinned: Vec<TokenStream>,
    /// Which payload positions travel that way. Set by `analyze::scan_pinned_args`
    /// after the `Ctx` exists, like `CtxEntry::raw`.
    pub(super) pinned: Vec<std::cell::Cell<bool>>,
    /// For a reference parameter, its pointee type, which is what a store for that
    /// position holds. Naming it at the store's creation is what keeps inference from
    /// having to chase the element type through a `push` in another arm.
    pub(super) param_pointees: Vec<Option<TokenStream>>,
    /// `: u64` per payload parameter, empty for an `impl Trait` one. Used to pin a
    /// payload argument hoisted to a temporary at the call site.
    pub(super) param_types: Vec<TokenStream>,
    /// The same types without the colon, where the type is needed on its own.
    pub(super) param_bare_types: Vec<TokenStream>,
}

pub(super) struct Ctx {
    /// Every function that shares this driver, in entry-variant order: the entry
    /// point for member `i` is `E{i}`, and lowered loops are numbered after
    /// all of them.
    pub(super) members: Vec<Member>,
    pub(super) counter: Cell<usize>,
    pub(super) loops: RefCell<Vec<PayloadPoint>>,
    /// One per recursive call site. Each becomes a variant of the frame enum and an
    /// arm of the body's `match` — which is what lets the driver keep frames in a
    /// plain `Vec` instead of boxing a closure per call.
    pub(super) resumes: RefCell<Vec<ResumePoint>>,
    /// The result bindings of the recursive calls we are currently *inside* the
    /// continuation of. They are in scope, but no `Env` knows that: they are bound
    /// by a closure the transform generates, not by the user's code. A loop needs
    /// them threaded all the same. The walk is depth-first and `k` is called
    /// synchronously, so this stack mirrors lexical scope exactly.
    pub(super) results: RefCell<Vec<Ident>>,
    /// Parameters lent out by the driver instead of travelling in the payload,
    /// in slot order. Shared by the whole group.
    pub(super) context: Vec<CtxEntry>,
    /// Are the members associated items? A module path then names something else, so
    /// `self::g(..)` in one of their bodies is not a call to a member. See `scope::edges`.
    pub(super) assoc: bool,
    /// `: R` for the driver's own result, naming the union when the members disagree.
    pub(super) ret_ann: TokenStream,
    /// Each member's return type as `: R`, empty for an `impl Trait` return. Used to annotate a
    /// resumed value once it has been taken out of the union — which is what lets method
    /// resolution inside a continuation see its receiver's type; left to inference,
    /// `f(n - 1).wrapping_add(1)` is E0689.
    pub(super) rets: Vec<TokenStream>,
    /// The same as a bare type, for slots rather than bindings. A resumed value's type is the
    /// callee's return type, and saying so is what lets a payload carrying that value be named --
    /// which matters when the only code that would have constructed it is `#[cfg]`-ed out and
    /// inference has nothing else to go on.
    pub(super) ret_types: Vec<TokenStream>,
    /// The union of the members' return types, when they differ. The driver has one
    /// result type, so a group whose members answer with different types answers with
    /// this instead, and each member's entry takes its own variant back out.
    pub(super) ret_union: Option<Ident>,
    pub(super) opts: Opts,
    /// Which member's body is being lowered; one at a time, so one slot suffices.
    pub(super) current: Cell<usize>,
    /// The `#[cfg]` predicates enclosing the code being lowered, outermost first. A recursive call
    /// under one is cut across the driver's arms, so each arm it produces has to carry the same
    /// gate -- and the frame variant it uses needs an arm for the other case, since the enum is
    /// declared whatever the predicate says.
    pub(super) gates: RefCell<Vec<TokenStream>>,
    /// Declared type of each annotated `let`, by `(member, name)`; `None` if bound
    /// inconsistently. Re-applied where a payload slot carries that local.
    pub(super) local_types: RefCell<HashMap<(usize, String), Option<TokenStream>>>,
    /// `(loop index, element type)` per `for` loop over a borrow: the collection moves into the
    /// store so the iterator borrows that, not a local the frame owns.
    pub(super) loop_stores: RefCell<Vec<(usize, TokenStream)>>,
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

    /// Everything in scope at this point: the user's bindings, then the result
    /// bindings of the continuations we are nested in.
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
    ///
    /// One store per position rather than one per function: each holds a different
    /// type, and a store's element type is a single inferred one.
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

    /// The element type a store holds, i.e. the pointee of that position's parameter.
    fn pin_element(&self, member: usize, position: usize) -> Option<TokenStream> {
        self.members[member].param_pointees[position].clone()
    }

    /// Each store's element type in slot order: parameter positions, then one per borrowing loop.
    pub(super) fn pin_elements(&self) -> Vec<Option<TokenStream>> {
        let params = self
            .pinned_positions()
            .iter()
            .map(|&(i, j)| self.pin_element(i, j))
            .collect::<Vec<_>>();
        let loops = self
            .loop_stores
            .borrow()
            .iter()
            .map(|(_, elem)| Some(elem.clone()))
            .collect::<Vec<_>>();
        params.into_iter().chain(loops).collect()
    }

    /// Reserve a borrowing loop's store, returning its context-tuple index. The element type is
    /// named here because `C` in `&mut C` settles before the closure body is checked.
    pub(super) fn loop_store_slot(&self, loop_idx: usize, elem: TokenStream) -> syn::Index {
        let mut stores = self.loop_stores.borrow_mut();
        let at = match stores.iter().position(|(n, _)| *n == loop_idx) {
            Some(at) => at,
            None => {
                stores.push((loop_idx, elem));
                stores.len() - 1
            }
        };
        syn::Index::from(self.context.len() + self.pinned_positions().len() + at)
    }

    /// Record an annotated `let` binding of the member being lowered.
    pub(super) fn note_local_type(&self, name: &Ident, ty: TokenStream) {
        let key = (self.current.get(), name.to_string());
        let rendered = ty.to_string();
        let mut map = self.local_types.borrow_mut();
        match map.get(&key) {
            // Shadowed with a different type: neither annotation describes the slot on its own.
            Some(Some(seen)) if seen.to_string() != rendered => {
                map.insert(key, None);
            }
            Some(_) => {}
            None => {
                map.insert(key, Some(ty));
            }
        }
    }

    /// Does this member own the value named by `e`, as far as the macro can tell?
    ///
    /// Only an annotated `let` of a non-reference type counts. A reference is rooted outside the
    /// driver and can be lent as it is; an unannotated local has no type to judge by, so it keeps
    /// the plain borrow and whatever error that brings.
    pub(super) fn owns_named_local(&self, member: usize, e: &syn::Expr) -> bool {
        let syn::Expr::Path(p) = e else { return false };
        let Some(name) = p.path.get_ident() else {
            return false;
        };
        if self.param_type_of(member, name).is_some() {
            // A parameter already travels in the payload; lend it as it is.
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

    /// The type the body binds a payload parameter to, if it is writable at all — not `impl Trait`.
    ///
    /// A pinned position has two types and this is the reference, not the `*const T`: the entry
    /// payload holds the pointer and the arm rebinds the name, so a loop state or frame carrying the
    /// name carries the rebinding. `emit::variant_payload_type` reads `pinned` for the pointer.
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

    /// The context-tuple index of the store for one position. The stores sit after
    /// the user's own context slots.
    pub(super) fn pin_slot(&self, member: usize, position: usize) -> syn::Index {
        let at = self
            .pinned_positions()
            .iter()
            .position(|&pair| pair == (member, position))
            .expect("only called for a pinned position");
        syn::Index::from(self.context.len() + at)
    }

    /// The context rebindings, in slot order. Emitted at the top of the body closure and of
    /// every continuation: a continuation must re-derive its bindings from its *own* lent
    /// context, never capture the enclosing one.
    pub(super) fn ctx_prologue(&self) -> TokenStream {
        let binds = self.context.iter().enumerate().map(|(i, e)| e.rebind(i));
        quote! { #(#binds)* }
    }

    /// Reserve an entry point for a loop; the code is filled in afterwards.
    pub(super) fn reserve_loop(
        &self,
        scope: Vec<Ident>,
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
        });
        loops.len() - 1
    }

    pub(super) fn set_loop_body(&self, idx: usize, body: TokenStream) {
        self.loops.borrow_mut()[idx].code = body;
    }

    /// How a resumed value is taken out of the union, if there is one. The callee is
    /// known at the call site, so the variant is too.
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

    /// The type of an expression the transform is about to bind to a temporary, where it can say:
    /// a bare binding has the type its parameter or its annotated `let` gave it. Nothing else is
    /// guessed. Used to keep a temporary's payload slot nameable, which is what a gated frame needs
    /// -- there, inference has no construction to work from.
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

    /// Reserve a resume point for a recursive call; the code is filled in once the
    /// continuation has been generated.
    pub(super) fn reserve_resume(
        &self,
        scope: Vec<Ident>,
        forced: Vec<Ident>,
        value: Ident,
    ) -> usize {
        let mut resumes = self.resumes.borrow_mut();
        resumes.push(ResumePoint {
            point: PayloadPoint {
                member: self.current.get(),
                scope,
                forced,
                code: TokenStream::new(),
                gates: self.gates.borrow().clone(),
            },
            value,
        });
        resumes.len() - 1
    }

    pub(super) fn set_resume_code(&self, idx: usize, code: TokenStream) {
        self.resumes.borrow_mut()[idx].point.code = code;
    }

    /// Which member this expression calls, if it is a call to one of them.
    /// After `desugar_receiver`, a method's `self.walk(a)` has already become
    /// `walk(a)`, so one shape covers both.
    pub(super) fn rec_call<'e>(&self, e: &'e Expr) -> Option<(usize, &'e syn::ExprCall)> {
        let Expr::Call(call) = e else { return None };
        let Expr::Path(p) = &*call.func else {
            return None;
        };
        let segments = &p.path.segments;
        // `self::g(..)` names this module's `g`, which is a member when the members are a module's
        // functions. Where they are an impl block's, that path names a free function instead.
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

/// A macro-level continuation: given tokens for the *value* produced at this
/// point, produce tokens for a `__SsStep` expression.
pub(super) type Cont<'a> = &'a dyn Fn(TokenStream) -> syn::Result<TokenStream>;

/// The innermost lowered loop, for rewriting `break` / `continue`.
pub(super) struct LoopCtx<'a> {
    /// Index among the lowered loops, which names its state placeholder.
    pub(super) idx: usize,
    /// Index of its entry *variant*: the members occupy the first ones, so
    /// this is not `idx`. `continue` becomes a `Tail` to it.
    pub(super) variant: usize,
    /// `break` runs the code that follows the loop.
    pub(super) brk: Cont<'a>,
}

/// What the transform needs to know at each point in the walk.
#[derive(Clone)]
pub(super) struct Env<'a> {
    /// Bindings in scope, in declaration order. Threading state through a loop
    /// entry point is a move, so order must be stable.
    pub(super) scope: Vec<Ident>,
    pub(super) lp: Option<&'a LoopCtx<'a>>,
    /// Assignments that must run before any escape from here — `?`, `return`,
    /// `break`, `continue`. Non-empty only while evaluating the arguments that
    /// follow a swapped context slot: leaving without putting the parent's pointer
    /// back would strand the slot pointing at the child.
    pub(super) restores: TokenStream,
    /// Stores to truncate before `?` or `return` abandons the member. Not `continue`, which stays
    /// in the loop; `break` releases through the continuation instead.
    pub(super) teardown: TokenStream,
    /// How this member's own result enters the union, when the group has one: the union
    /// type and this member's variant. `return` and `?` finish the member from wherever
    /// they stand, so they have to wrap the value just as a normal exit does.
    pub(super) wrap: Option<(Ident, Ident)>,
}

impl Env<'_> {
    /// A value as this member's result: wrapped into the union if there is one. `return`
    /// and `?` finish the member from wherever they stand, so they wrap just as a tail
    /// expression does.
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
            if !next.scope.iter().any(|i| i == &id) {
                next.scope.push(id);
            }
        }
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
        }
    }

    /// Evaluating an argument after a swap: an escape has to undo it first.
    /// Add a truncation to run before `?` or `return` leaves the member.
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
